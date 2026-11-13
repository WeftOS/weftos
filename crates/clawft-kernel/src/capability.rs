//! Agent capabilities and resource limits.
//!
//! Defines the permission model for kernel-managed agents. Each agent
//! process has an [`AgentCapabilities`] that governs what IPC scopes,
//! tool categories, and resource budgets the agent is allowed.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::KernelError;
use crate::process::{Pid, ProcessTable};

/// Resource limits for an agent process.
///
/// Enforced by the kernel's process table when updating resource
/// usage counters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourceLimits {
    /// Maximum memory (in bytes) the agent is allowed to consume.
    #[serde(default = "default_max_memory", alias = "maxMemoryBytes")]
    pub max_memory_bytes: u64,

    /// Maximum CPU time (in milliseconds) before the agent is killed.
    #[serde(default = "default_max_cpu", alias = "maxCpuTimeMs")]
    pub max_cpu_time_ms: u64,

    /// Maximum number of tool calls the agent may make.
    #[serde(default = "default_max_tool_calls", alias = "maxToolCalls")]
    pub max_tool_calls: u64,

    /// Maximum number of IPC messages the agent may send.
    #[serde(default = "default_max_messages", alias = "maxMessages")]
    pub max_messages: u64,

    /// Maximum disk usage in bytes for this agent (K1-G4).
    ///
    /// Enforced when writing to the resource tree under `/agents/{agent_id}/`.
    /// Default: 100 MiB. Set to 0 for unlimited.
    #[cfg(feature = "os-patterns")]
    #[serde(default = "default_max_disk", alias = "maxDiskBytes")]
    pub max_disk_bytes: u64,
}

fn default_max_memory() -> u64 {
    256 * 1024 * 1024 // 256 MiB
}

fn default_max_cpu() -> u64 {
    300_000 // 5 minutes
}

fn default_max_tool_calls() -> u64 {
    1000
}

fn default_max_messages() -> u64 {
    5000
}

#[cfg(feature = "os-patterns")]
fn default_max_disk() -> u64 {
    100 * 1024 * 1024 // 100 MiB
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_memory_bytes: default_max_memory(),
            max_cpu_time_ms: default_max_cpu(),
            max_tool_calls: default_max_tool_calls(),
            max_messages: default_max_messages(),
            #[cfg(feature = "os-patterns")]
            max_disk_bytes: default_max_disk(),
        }
    }
}

/// Topics browser / [`IpcScope::Restricted`] agents may access without elevation.
///
/// A topic is public when it is exactly `"public"` or has the `public.` prefix
/// (e.g. `public.events`, `public.health`). All other topics require a broader
/// IPC scope (typically granted via governance-gated capability elevation).
pub fn is_public_topic(topic: &str) -> bool {
    topic == "public" || topic.starts_with("public.")
}

/// IPC scope defining which message targets an agent may communicate with.
#[non_exhaustive]
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum IpcScope {
    /// Agent may communicate with all other agents.
    #[default]
    All,
    /// Agent may only communicate with its parent.
    ParentOnly,
    /// Agent may communicate with a specified set of PIDs.
    ///
    /// Topic pub/sub is limited to [public topics](is_public_topic) only
    /// (S7 / WEFT-108 browser sandbox default).
    Restricted(Vec<u64>),
    /// Agent may only publish/subscribe to specified topics (no direct PID messaging).
    Topic(Vec<String>),
    /// Agent may not send IPC messages.
    None,
}

/// Capabilities assigned to an agent process.
///
/// Governs what the agent is allowed to do within the kernel.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentCapabilities {
    /// Whether the agent can spawn child processes.
    #[serde(default = "default_true", alias = "canSpawn")]
    pub can_spawn: bool,

    /// Whether the agent can send/receive IPC messages.
    #[serde(default = "default_true", alias = "canIpc")]
    pub can_ipc: bool,

    /// Whether the agent can execute tools.
    #[serde(default = "default_true", alias = "canExecTools")]
    pub can_exec_tools: bool,

    /// Whether the agent can make network requests.
    #[serde(default, alias = "canNetwork")]
    pub can_network: bool,

    /// IPC scope restriction.
    #[serde(default, alias = "ipcScope")]
    pub ipc_scope: IpcScope,

    /// Resource limits for this agent.
    #[serde(default, alias = "resourceLimits")]
    pub resource_limits: ResourceLimits,
}

fn default_true() -> bool {
    true
}

impl Default for AgentCapabilities {
    fn default() -> Self {
        Self {
            can_spawn: true,
            can_ipc: true,
            can_exec_tools: true,
            can_network: false,
            ipc_scope: IpcScope::default(),
            resource_limits: ResourceLimits::default(),
        }
    }
}

impl AgentCapabilities {
    /// Create capabilities for a browser-platform agent.
    ///
    /// Browser agents default to restricted IPC scope (empty allow-list),
    /// no spawning, no network access, and no shell — maximising the
    /// sandbox surface for untrusted in-browser code.
    pub fn browser_default() -> Self {
        Self {
            can_spawn: false,
            can_ipc: true,
            can_exec_tools: true,
            can_network: false,
            ipc_scope: IpcScope::Restricted(vec![]),
            resource_limits: ResourceLimits {
                max_memory_bytes: 64 * 1024 * 1024, // 64 MiB
                max_cpu_time_ms: 60_000,            // 1 minute
                max_tool_calls: 200,
                max_messages: 500,
                #[cfg(feature = "os-patterns")]
                max_disk_bytes: 10 * 1024 * 1024, // 10 MiB for browser agents
            },
        }
    }

    /// Check whether the agent is allowed to send a message to the given PID.
    pub fn can_message(&self, target_pid: u64) -> bool {
        if !self.can_ipc {
            return false;
        }
        match &self.ipc_scope {
            IpcScope::All => true,
            IpcScope::ParentOnly => false, // Caller must check parent separately
            IpcScope::Restricted(pids) => pids.contains(&target_pid),
            IpcScope::Topic(_) => false, // Topic-scoped agents cannot direct-message PIDs
            IpcScope::None => false,
        }
    }

    /// Check whether the agent is allowed to publish/subscribe to a topic.
    ///
    /// [`IpcScope::Restricted`] (browser default) only permits
    /// [public topics](is_public_topic). Other scopes are unchanged.
    pub fn can_topic(&self, topic: &str) -> bool {
        if !self.can_ipc {
            return false;
        }
        match &self.ipc_scope {
            IpcScope::All => true,
            IpcScope::Topic(topics) => topics.iter().any(|t| t == topic),
            IpcScope::ParentOnly => true, // topic access not restricted for parent-only
            IpcScope::Restricted(_) => is_public_topic(topic),
            IpcScope::None => false,
        }
    }

    /// Check whether the resource usage is within limits.
    pub fn within_limits(&self, memory: u64, cpu: u64, tools: u64, msgs: u64) -> bool {
        memory <= self.resource_limits.max_memory_bytes
            && cpu <= self.resource_limits.max_cpu_time_ms
            && tools <= self.resource_limits.max_tool_calls
            && msgs <= self.resource_limits.max_messages
    }
}

/// Sandbox policy governing filesystem and shell access.
///
/// Controls whether an agent can execute shell commands, access
/// the network, and which filesystem paths are allowed or denied.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SandboxPolicy {
    /// Whether the agent may execute shell commands.
    #[serde(default, alias = "allowShell")]
    pub allow_shell: bool,

    /// Whether the agent may make network requests.
    #[serde(default, alias = "allowNetwork")]
    pub allow_network: bool,

    /// Filesystem paths the agent is allowed to access.
    #[serde(default, alias = "allowedPaths")]
    pub allowed_paths: Vec<String>,

    /// Filesystem paths the agent is explicitly denied from accessing.
    #[serde(default, alias = "deniedPaths")]
    pub denied_paths: Vec<String>,
}

/// Tool-level permission configuration.
///
/// An allow/deny list model where deny overrides allow.
/// Empty `allow` means all tools permitted (unless explicitly denied).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolPermissions {
    /// Tools the agent is allowed to use (empty = all allowed).
    #[serde(default, alias = "tools")]
    pub allow: Vec<String>,

    /// Tools the agent is explicitly denied from using.
    #[serde(default, alias = "denyTools")]
    pub deny: Vec<String>,

    /// Named services the agent may access (e.g. "memory", "cron").
    #[serde(default, alias = "serviceAccess")]
    pub service_access: Vec<String>,
}

/// Resource type for capability limit checks.
///
/// Each variant carries the current value to check against limits.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum ResourceType {
    /// Memory usage in bytes.
    Memory(u64),
    /// CPU time in milliseconds.
    CpuTime(u64),
    /// Number of concurrent tool calls.
    ConcurrentTools(u32),
    /// Number of IPC messages sent.
    Messages(u64),
}

/// Capability checker that enforces per-agent access control.
///
/// The checker reads capabilities from the process table and
/// validates tool access, IPC routing, service access, and
/// resource limits. It is designed to be called from tool
/// execution hooks without requiring a direct dependency on
/// the kernel crate (via trait objects in the core).
pub struct CapabilityChecker {
    process_table: Arc<ProcessTable>,
}

impl CapabilityChecker {
    /// Create a new capability checker backed by the given process table.
    pub fn new(process_table: Arc<ProcessTable>) -> Self {
        Self { process_table }
    }

    /// Check whether a process is allowed to call a tool.
    ///
    /// Evaluation order:
    /// 1. Agent must have `can_exec_tools` enabled.
    /// 2. If `tool_permissions.deny` is non-empty, the tool must not
    ///    be in the deny list (deny overrides allow).
    /// 3. If `tool_permissions.allow` is non-empty, the tool must be
    ///    in the allow list.
    /// 4. Shell tools require `sandbox.allow_shell`.
    ///
    /// # Errors
    ///
    /// Returns `KernelError::CapabilityDenied` with a description
    /// of why access was denied.
    pub fn check_tool_access(
        &self,
        pid: Pid,
        tool_name: &str,
        tool_permissions: Option<&ToolPermissions>,
        sandbox: Option<&SandboxPolicy>,
    ) -> Result<(), KernelError> {
        let entry = self
            .process_table
            .get(pid)
            .ok_or(KernelError::ProcessNotFound { pid })?;

        // Must have tool execution capability
        if !entry.capabilities.can_exec_tools {
            return Err(KernelError::CapabilityDenied {
                pid,
                action: format!("execute tool '{tool_name}'"),
                reason: "agent does not have can_exec_tools capability".into(),
            });
        }

        // Check deny list (deny overrides allow)
        if let Some(perms) = tool_permissions {
            if perms.deny.iter().any(|d| d == tool_name) {
                return Err(KernelError::CapabilityDenied {
                    pid,
                    action: format!("execute tool '{tool_name}'"),
                    reason: "tool is in the deny list".into(),
                });
            }

            // Check allow list (empty = all allowed)
            if !perms.allow.is_empty() && !perms.allow.iter().any(|a| a == tool_name) {
                return Err(KernelError::CapabilityDenied {
                    pid,
                    action: format!("execute tool '{tool_name}'"),
                    reason: "tool is not in the allow list".into(),
                });
            }
        }

        // Check sandbox policy for shell tools
        if let Some(sb) = sandbox
            && is_shell_tool(tool_name)
            && !sb.allow_shell
        {
            return Err(KernelError::CapabilityDenied {
                pid,
                action: format!("execute shell tool '{tool_name}'"),
                reason: "sandbox policy does not allow shell execution".into(),
            });
        }

        Ok(())
    }

    /// Check whether a process may send a message to another process.
    ///
    /// Uses the sender's IPC scope to determine if communication
    /// with the target PID is allowed.
    ///
    /// # Errors
    ///
    /// Returns `KernelError::CapabilityDenied` if IPC is disabled
    /// or the target is outside the sender's IPC scope.
    pub fn check_ipc_target(&self, from_pid: Pid, to_pid: Pid) -> Result<(), KernelError> {
        let entry = self
            .process_table
            .get(from_pid)
            .ok_or(KernelError::ProcessNotFound { pid: from_pid })?;

        if !entry.capabilities.can_ipc {
            return Err(KernelError::CapabilityDenied {
                pid: from_pid,
                action: format!("send IPC message to PID {to_pid}"),
                reason: "agent does not have IPC capability".into(),
            });
        }

        if !entry.capabilities.can_message(to_pid) {
            return Err(KernelError::CapabilityDenied {
                pid: from_pid,
                action: format!("send IPC message to PID {to_pid}"),
                reason: format!(
                    "target PID {to_pid} is outside IPC scope {:?}",
                    entry.capabilities.ipc_scope
                ),
            });
        }

        Ok(())
    }

    /// Check whether a process may publish or subscribe to a topic.
    ///
    /// # Errors
    ///
    /// Returns `KernelError::CapabilityDenied` if the agent's IPC
    /// scope does not permit the given topic.
    pub fn check_ipc_topic(&self, pid: Pid, topic: &str) -> Result<(), KernelError> {
        let entry = self
            .process_table
            .get(pid)
            .ok_or(KernelError::ProcessNotFound { pid })?;

        if !entry.capabilities.can_topic(topic) {
            return Err(KernelError::CapabilityDenied {
                pid,
                action: format!("access topic '{topic}'"),
                reason: format!(
                    "topic '{topic}' is outside IPC scope {:?}",
                    entry.capabilities.ipc_scope
                ),
            });
        }

        Ok(())
    }

    /// Check whether a process may access a named service.
    ///
    /// If `tool_permissions` has a non-empty `service_access` list,
    /// the service name must appear in it.
    ///
    /// # Errors
    ///
    /// Returns `KernelError::CapabilityDenied` if the service is not
    /// in the agent's service access list.
    pub fn check_service_access(
        &self,
        pid: Pid,
        service_name: &str,
        tool_permissions: Option<&ToolPermissions>,
    ) -> Result<(), KernelError> {
        // Verify the PID exists
        let _entry = self
            .process_table
            .get(pid)
            .ok_or(KernelError::ProcessNotFound { pid })?;

        if let Some(perms) = tool_permissions
            && !perms.service_access.is_empty()
            && !perms.service_access.iter().any(|s| s == service_name)
        {
            return Err(KernelError::CapabilityDenied {
                pid,
                action: format!("access service '{service_name}'"),
                reason: "service is not in the agent's service access list".into(),
            });
        }

        Ok(())
    }

    /// Check whether a resource usage is within the agent's limits.
    ///
    /// # Errors
    ///
    /// Returns `KernelError::ResourceLimitExceeded` if the resource
    /// usage exceeds the agent's configured limits.
    pub fn check_resource_limit(
        &self,
        pid: Pid,
        resource: &ResourceType,
    ) -> Result<(), KernelError> {
        let entry = self
            .process_table
            .get(pid)
            .ok_or(KernelError::ProcessNotFound { pid })?;

        let limits = &entry.capabilities.resource_limits;

        match resource {
            ResourceType::Memory(bytes) => {
                if *bytes > limits.max_memory_bytes {
                    return Err(KernelError::ResourceLimitExceeded {
                        pid,
                        resource: "memory".into(),
                        current: *bytes,
                        limit: limits.max_memory_bytes,
                    });
                }
            }
            ResourceType::CpuTime(ms) => {
                if *ms > limits.max_cpu_time_ms {
                    return Err(KernelError::ResourceLimitExceeded {
                        pid,
                        resource: "cpu_time".into(),
                        current: *ms,
                        limit: limits.max_cpu_time_ms,
                    });
                }
            }
            ResourceType::ConcurrentTools(count) => {
                if u64::from(*count) > limits.max_tool_calls {
                    return Err(KernelError::ResourceLimitExceeded {
                        pid,
                        resource: "concurrent_tools".into(),
                        current: u64::from(*count),
                        limit: limits.max_tool_calls,
                    });
                }
            }
            ResourceType::Messages(count) => {
                if *count > limits.max_messages {
                    return Err(KernelError::ResourceLimitExceeded {
                        pid,
                        resource: "messages".into(),
                        current: *count,
                        limit: limits.max_messages,
                    });
                }
            }
        }

        Ok(())
    }

    /// Get a reference to the underlying process table.
    pub fn process_table(&self) -> &Arc<ProcessTable> {
        &self.process_table
    }
}

// ── Browser capability elevation via governance gate ────────────────

/// Request to elevate a browser agent's capabilities.
///
/// Browser agents start with a restricted sandbox. Elevation requires
/// governance gate approval before additional permissions are granted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityElevationRequest {
    /// PID of the agent requesting elevation.
    pub pid: u64,
    /// Current capabilities.
    pub current: AgentCapabilities,
    /// Requested elevated capabilities.
    pub requested: AgentCapabilities,
    /// Justification for elevation.
    pub reason: String,
}

/// Result of a capability elevation request.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum ElevationResult {
    /// Elevation granted.
    Granted { new_capabilities: AgentCapabilities },
    /// Elevation denied by governance gate.
    Denied { reason: String },
}

impl AgentCapabilities {
    /// Build an elevation request from current to requested capabilities.
    /// The `pid` field is set to 0 and must be filled by the caller.
    pub fn request_elevation(
        current: &AgentCapabilities,
        requested: &AgentCapabilities,
        platform: &str,
    ) -> CapabilityElevationRequest {
        CapabilityElevationRequest {
            pid: 0, // filled by caller
            current: current.clone(),
            requested: requested.clone(),
            reason: format!("capability elevation for {platform} agent"),
        }
    }

    /// Build an elevation request with governance gating and chain logging.
    ///
    /// If a governance gate is provided, the request is checked against
    /// the gate policy. If denied, returns `ElevationResult::Denied`.
    /// If a chain manager is provided, the elevation attempt is logged.
    #[cfg(feature = "exochain")]
    pub fn request_elevation_gated(
        _current: &AgentCapabilities,
        requested: &AgentCapabilities,
        platform: &str,
        pid: u64,
        gate: Option<&crate::gate::GovernanceGate>,
        chain: Option<&crate::chain::ChainManager>,
    ) -> ElevationResult {
        // Governance gate: check policy before allowing elevation.
        if let Some(gate) = gate {
            use crate::gate::GateBackend;
            let decision = gate.check(
                &format!("pid:{pid}"),
                "capability.elevate",
                &serde_json::json!({
                    "pid": pid,
                    "platform": platform,
                    "can_spawn": requested.can_spawn,
                    "can_network": requested.can_network,
                    "effect": { "risk": 0.5, "security": 0.5 },
                }),
            );
            if decision.is_deny() {
                // Chain logging: record denied elevation.
                if let Some(cm) = chain {
                    cm.append(
                        "capability",
                        crate::chain::EVENT_KIND_CAPABILITY_ELEVATE,
                        Some(serde_json::json!({
                            "pid": pid,
                            "platform": platform,
                            "result": "denied",
                            "reason": "governance gate denied elevation",
                        })),
                    );
                }
                return ElevationResult::Denied {
                    reason: "governance denied capability elevation".into(),
                };
            }
        }

        // Chain logging: record approved elevation.
        if let Some(cm) = chain {
            cm.append(
                "capability",
                crate::chain::EVENT_KIND_CAPABILITY_ELEVATE,
                Some(serde_json::json!({
                    "pid": pid,
                    "platform": platform,
                    "result": "granted",
                    "can_spawn": requested.can_spawn,
                    "can_network": requested.can_network,
                })),
            );
        }

        ElevationResult::Granted {
            new_capabilities: requested.clone(),
        }
    }

    /// Check if elevation is needed (browser agents start restricted).
    ///
    /// Returns `true` if the platform is `"browser"` and the requested
    /// capabilities exceed the browser sandbox defaults (spawn, network,
    /// or non-restricted IPC scope).
    pub fn needs_elevation(platform: &str, requested: &AgentCapabilities) -> bool {
        platform == "browser"
            && (requested.can_spawn
                || requested.can_network
                || !matches!(requested.ipc_scope, IpcScope::Restricted(_)))
    }
}

/// Check whether a tool name is a shell execution tool.
fn is_shell_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "shell_exec" | "exec_shell" | "bash" | "command" | "run_command"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_capabilities() {
        let caps = AgentCapabilities::default();
        assert!(caps.can_spawn);
        assert!(caps.can_ipc);
        assert!(caps.can_exec_tools);
        assert!(!caps.can_network);
        assert_eq!(caps.ipc_scope, IpcScope::All);
    }

    #[test]
    fn default_resource_limits() {
        let limits = ResourceLimits::default();
        assert_eq!(limits.max_memory_bytes, 256 * 1024 * 1024);
        assert_eq!(limits.max_cpu_time_ms, 300_000);
        assert_eq!(limits.max_tool_calls, 1000);
        assert_eq!(limits.max_messages, 5000);
    }

    #[test]
    fn can_message_all_scope() {
        let caps = AgentCapabilities::default();
        assert!(caps.can_message(1));
        assert!(caps.can_message(999));
    }

    #[test]
    fn can_message_restricted_scope() {
        let caps = AgentCapabilities {
            ipc_scope: IpcScope::Restricted(vec![1, 2, 3]),
            ..Default::default()
        };
        assert!(caps.can_message(1));
        assert!(caps.can_message(2));
        assert!(!caps.can_message(4));
    }

    #[test]
    fn can_message_none_scope() {
        let caps = AgentCapabilities {
            ipc_scope: IpcScope::None,
            ..Default::default()
        };
        assert!(!caps.can_message(1));
    }

    #[test]
    fn can_message_topic_scope_blocks_direct() {
        let caps = AgentCapabilities {
            ipc_scope: IpcScope::Topic(vec!["build".into(), "deploy".into()]),
            ..Default::default()
        };
        // Topic-scoped agents cannot direct-message PIDs
        assert!(!caps.can_message(1));
        assert!(!caps.can_message(999));
    }

    #[test]
    fn can_topic_with_topic_scope() {
        let caps = AgentCapabilities {
            ipc_scope: IpcScope::Topic(vec!["build".into(), "deploy".into()]),
            ..Default::default()
        };
        assert!(caps.can_topic("build"));
        assert!(caps.can_topic("deploy"));
        assert!(!caps.can_topic("admin"));
    }

    #[test]
    fn can_topic_with_all_scope() {
        let caps = AgentCapabilities::default(); // IpcScope::All
        assert!(caps.can_topic("anything"));
    }

    #[test]
    fn can_topic_with_none_scope() {
        let caps = AgentCapabilities {
            ipc_scope: IpcScope::None,
            ..Default::default()
        };
        assert!(!caps.can_topic("build"));
    }

    #[test]
    fn can_message_ipc_disabled() {
        let caps = AgentCapabilities {
            can_ipc: false,
            ..Default::default()
        };
        assert!(!caps.can_message(1));
    }

    #[test]
    fn within_limits_ok() {
        let caps = AgentCapabilities::default();
        assert!(caps.within_limits(1000, 1000, 10, 10));
    }

    #[test]
    fn within_limits_exceeded() {
        let caps = AgentCapabilities {
            resource_limits: ResourceLimits {
                max_memory_bytes: 100,
                max_cpu_time_ms: 100,
                max_tool_calls: 5,
                max_messages: 5,
                #[cfg(feature = "os-patterns")]
                max_disk_bytes: 100,
            },
            ..Default::default()
        };
        assert!(!caps.within_limits(200, 50, 3, 3)); // memory exceeded
        assert!(!caps.within_limits(50, 200, 3, 3)); // cpu exceeded
        assert!(!caps.within_limits(50, 50, 10, 3)); // tools exceeded
        assert!(!caps.within_limits(50, 50, 3, 10)); // messages exceeded
    }

    #[test]
    fn serde_roundtrip_capabilities() {
        let caps = AgentCapabilities {
            can_spawn: false,
            can_ipc: true,
            can_exec_tools: false,
            can_network: true,
            ipc_scope: IpcScope::Restricted(vec![1, 2]),
            resource_limits: ResourceLimits {
                max_memory_bytes: 1024,
                max_cpu_time_ms: 500,
                max_tool_calls: 10,
                max_messages: 20,
                #[cfg(feature = "os-patterns")]
                max_disk_bytes: 2048,
            },
        };
        let json = serde_json::to_string(&caps).unwrap();
        let restored: AgentCapabilities = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, caps);
    }

    #[test]
    fn deserialize_empty_capabilities() {
        let caps: AgentCapabilities = serde_json::from_str("{}").unwrap();
        assert!(caps.can_spawn);
        assert!(caps.can_ipc);
        assert!(caps.can_exec_tools);
        assert!(!caps.can_network);
    }

    // ── SandboxPolicy tests ──────────────────────────────────────────

    #[test]
    fn sandbox_policy_default() {
        let sb = SandboxPolicy::default();
        assert!(!sb.allow_shell);
        assert!(!sb.allow_network);
        assert!(sb.allowed_paths.is_empty());
        assert!(sb.denied_paths.is_empty());
    }

    #[test]
    fn sandbox_policy_serde_roundtrip() {
        let sb = SandboxPolicy {
            allow_shell: true,
            allow_network: false,
            allowed_paths: vec!["/workspace".into()],
            denied_paths: vec!["/etc".into(), "/root".into()],
        };
        let json = serde_json::to_string(&sb).unwrap();
        let restored: SandboxPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, sb);
    }

    // ── ToolPermissions tests ────────────────────────────────────────

    #[test]
    fn tool_permissions_default() {
        let perms = ToolPermissions::default();
        assert!(perms.allow.is_empty());
        assert!(perms.deny.is_empty());
        assert!(perms.service_access.is_empty());
    }

    #[test]
    fn tool_permissions_serde_roundtrip() {
        let perms = ToolPermissions {
            allow: vec!["read_file".into(), "write_file".into()],
            deny: vec!["shell_exec".into()],
            service_access: vec!["memory".into()],
        };
        let json = serde_json::to_string(&perms).unwrap();
        let restored: ToolPermissions = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, perms);
    }

    // ── CapabilityChecker tests ──────────────────────────────────────

    use crate::process::{ProcessEntry, ProcessState, ResourceUsage};
    use tokio_util::sync::CancellationToken;

    fn make_checker_with_entry(caps: AgentCapabilities) -> (CapabilityChecker, Pid) {
        let table = Arc::new(ProcessTable::new(16));
        let entry = ProcessEntry {
            pid: 0,
            agent_id: "test-agent".to_owned(),
            state: ProcessState::Running,
            capabilities: caps,
            resource_usage: ResourceUsage::default(),
            cancel_token: CancellationToken::new(),
            parent_pid: None,
        };
        let pid = table.insert(entry).unwrap();
        (CapabilityChecker::new(table), pid)
    }

    #[test]
    fn checker_tool_access_allowed_by_default() {
        let (checker, pid) = make_checker_with_entry(AgentCapabilities::default());
        assert!(
            checker
                .check_tool_access(pid, "read_file", None, None)
                .is_ok()
        );
    }

    #[test]
    fn checker_tool_access_denied_no_exec_tools() {
        let caps = AgentCapabilities {
            can_exec_tools: false,
            ..Default::default()
        };
        let (checker, pid) = make_checker_with_entry(caps);
        let result = checker.check_tool_access(pid, "read_file", None, None);
        assert!(result.is_err());
    }

    #[test]
    fn checker_tool_deny_list_blocks() {
        let (checker, pid) = make_checker_with_entry(AgentCapabilities::default());
        let perms = ToolPermissions {
            deny: vec!["shell_exec".into()],
            ..Default::default()
        };
        let result = checker.check_tool_access(pid, "shell_exec", Some(&perms), None);
        assert!(result.is_err());
    }

    #[test]
    fn checker_tool_deny_overrides_allow() {
        let (checker, pid) = make_checker_with_entry(AgentCapabilities::default());
        let perms = ToolPermissions {
            allow: vec!["shell_exec".into()],
            deny: vec!["shell_exec".into()],
            ..Default::default()
        };
        let result = checker.check_tool_access(pid, "shell_exec", Some(&perms), None);
        assert!(result.is_err());
    }

    #[test]
    fn checker_tool_allow_list_restricts() {
        let (checker, pid) = make_checker_with_entry(AgentCapabilities::default());
        let perms = ToolPermissions {
            allow: vec!["read_file".into(), "write_file".into()],
            ..Default::default()
        };
        assert!(
            checker
                .check_tool_access(pid, "read_file", Some(&perms), None)
                .is_ok()
        );
        assert!(
            checker
                .check_tool_access(pid, "web_search", Some(&perms), None)
                .is_err()
        );
    }

    #[test]
    fn checker_sandbox_blocks_shell() {
        let (checker, pid) = make_checker_with_entry(AgentCapabilities::default());
        let sb = SandboxPolicy {
            allow_shell: false,
            ..Default::default()
        };
        let result = checker.check_tool_access(pid, "shell_exec", None, Some(&sb));
        assert!(result.is_err());
    }

    #[test]
    fn checker_sandbox_allows_shell() {
        let (checker, pid) = make_checker_with_entry(AgentCapabilities::default());
        let sb = SandboxPolicy {
            allow_shell: true,
            ..Default::default()
        };
        assert!(
            checker
                .check_tool_access(pid, "shell_exec", None, Some(&sb))
                .is_ok()
        );
    }

    #[test]
    fn checker_ipc_allowed() {
        let table = Arc::new(ProcessTable::new(16));
        let entry1 = ProcessEntry {
            pid: 0,
            agent_id: "sender".to_owned(),
            state: ProcessState::Running,
            capabilities: AgentCapabilities::default(), // IpcScope::All
            resource_usage: ResourceUsage::default(),
            cancel_token: CancellationToken::new(),
            parent_pid: None,
        };
        let entry2 = ProcessEntry {
            pid: 0,
            agent_id: "receiver".to_owned(),
            state: ProcessState::Running,
            capabilities: AgentCapabilities::default(),
            resource_usage: ResourceUsage::default(),
            cancel_token: CancellationToken::new(),
            parent_pid: None,
        };
        let pid1 = table.insert(entry1).unwrap();
        let pid2 = table.insert(entry2).unwrap();

        let checker = CapabilityChecker::new(table);
        assert!(checker.check_ipc_target(pid1, pid2).is_ok());
    }

    #[test]
    fn checker_ipc_denied_no_ipc() {
        let caps = AgentCapabilities {
            can_ipc: false,
            ..Default::default()
        };
        let (checker, pid) = make_checker_with_entry(caps);
        let result = checker.check_ipc_target(pid, 999);
        assert!(result.is_err());
    }

    #[test]
    fn checker_ipc_denied_restricted_scope() {
        let caps = AgentCapabilities {
            ipc_scope: IpcScope::Restricted(vec![5, 10]),
            ..Default::default()
        };
        let (checker, pid) = make_checker_with_entry(caps);
        assert!(checker.check_ipc_target(pid, 5).is_ok());
        assert!(checker.check_ipc_target(pid, 10).is_ok());
        assert!(checker.check_ipc_target(pid, 15).is_err());
    }

    #[test]
    fn checker_service_access_allowed_empty_list() {
        let (checker, pid) = make_checker_with_entry(AgentCapabilities::default());
        // Empty service_access = all services allowed
        let perms = ToolPermissions::default();
        assert!(
            checker
                .check_service_access(pid, "memory", Some(&perms))
                .is_ok()
        );
    }

    #[test]
    fn checker_service_access_restricted() {
        let (checker, pid) = make_checker_with_entry(AgentCapabilities::default());
        let perms = ToolPermissions {
            service_access: vec!["memory".into(), "cron".into()],
            ..Default::default()
        };
        assert!(
            checker
                .check_service_access(pid, "memory", Some(&perms))
                .is_ok()
        );
        assert!(
            checker
                .check_service_access(pid, "cron", Some(&perms))
                .is_ok()
        );
        assert!(
            checker
                .check_service_access(pid, "network", Some(&perms))
                .is_err()
        );
    }

    #[test]
    fn checker_resource_limit_memory_ok() {
        let (checker, pid) = make_checker_with_entry(AgentCapabilities::default());
        assert!(
            checker
                .check_resource_limit(pid, &ResourceType::Memory(1024))
                .is_ok()
        );
    }

    #[test]
    fn checker_resource_limit_memory_exceeded() {
        let caps = AgentCapabilities {
            resource_limits: ResourceLimits {
                max_memory_bytes: 100,
                ..Default::default()
            },
            ..Default::default()
        };
        let (checker, pid) = make_checker_with_entry(caps);
        let result = checker.check_resource_limit(pid, &ResourceType::Memory(200));
        assert!(result.is_err());
    }

    #[test]
    fn checker_resource_limit_cpu_exceeded() {
        let caps = AgentCapabilities {
            resource_limits: ResourceLimits {
                max_cpu_time_ms: 100,
                ..Default::default()
            },
            ..Default::default()
        };
        let (checker, pid) = make_checker_with_entry(caps);
        let result = checker.check_resource_limit(pid, &ResourceType::CpuTime(200));
        assert!(result.is_err());
    }

    #[test]
    fn checker_resource_limit_messages_exceeded() {
        let caps = AgentCapabilities {
            resource_limits: ResourceLimits {
                max_messages: 10,
                ..Default::default()
            },
            ..Default::default()
        };
        let (checker, pid) = make_checker_with_entry(caps);
        let result = checker.check_resource_limit(pid, &ResourceType::Messages(20));
        assert!(result.is_err());
    }

    #[test]
    fn checker_ipc_topic_allowed() {
        let caps = AgentCapabilities {
            ipc_scope: IpcScope::Topic(vec!["build".into(), "deploy".into()]),
            ..Default::default()
        };
        let (checker, pid) = make_checker_with_entry(caps);
        assert!(checker.check_ipc_topic(pid, "build").is_ok());
        assert!(checker.check_ipc_topic(pid, "deploy").is_ok());
        assert!(checker.check_ipc_topic(pid, "admin").is_err());
    }

    #[test]
    fn checker_ipc_topic_denied_for_direct_messaging() {
        let caps = AgentCapabilities {
            ipc_scope: IpcScope::Topic(vec!["build".into()]),
            ..Default::default()
        };
        let (checker, pid) = make_checker_with_entry(caps);
        // Topic-scoped agents cannot direct-message PIDs
        assert!(checker.check_ipc_target(pid, 999).is_err());
    }

    #[test]
    fn checker_nonexistent_pid() {
        let table = Arc::new(ProcessTable::new(16));
        let checker = CapabilityChecker::new(table);
        assert!(
            checker
                .check_tool_access(999, "read_file", None, None)
                .is_err()
        );
        assert!(checker.check_ipc_target(999, 1).is_err());
        assert!(
            checker
                .check_resource_limit(999, &ResourceType::Memory(0))
                .is_err()
        );
    }

    #[test]
    fn browser_default_uses_restricted_ipc() {
        let caps = AgentCapabilities::browser_default();
        assert!(
            matches!(caps.ipc_scope, IpcScope::Restricted(ref pids) if pids.is_empty()),
            "browser agents must default to IpcScope::Restricted([])"
        );
        assert!(!caps.can_spawn, "browser agents must not spawn");
        assert!(!caps.can_network, "browser agents must not access network");
        assert!(caps.can_ipc, "browser agents need IPC for kernel comms");
        assert!(caps.can_exec_tools, "browser agents need tool execution");
        // Tighter resource limits than default
        assert!(caps.resource_limits.max_memory_bytes < ResourceLimits::default().max_memory_bytes);
        assert!(caps.resource_limits.max_cpu_time_ms < ResourceLimits::default().max_cpu_time_ms);
    }

    #[test]
    fn browser_default_blocks_direct_messages() {
        let caps = AgentCapabilities::browser_default();
        // Empty restricted list means no PIDs are reachable
        assert!(!caps.can_message(1));
        assert!(!caps.can_message(999));
    }

    #[test]
    fn is_public_topic_prefix_rules() {
        assert!(is_public_topic("public"));
        assert!(is_public_topic("public.events"));
        assert!(is_public_topic("public.health.status"));
        assert!(!is_public_topic("publicx"));
        assert!(!is_public_topic("admin.secrets"));
        assert!(!is_public_topic("mesh.internal"));
        assert!(!is_public_topic(""));
    }

    /// WEFT-108 / S7: browser nodes with Restricted IPC cannot subscribe
    /// to non-public topics.
    #[test]
    fn browser_node_cannot_subscribe_to_non_public_topics() {
        let caps = AgentCapabilities::browser_default();
        assert!(
            matches!(caps.ipc_scope, IpcScope::Restricted(ref pids) if pids.is_empty()),
            "browser default must be IpcScope::Restricted([])"
        );

        // Public topics are allowed
        assert!(caps.can_topic("public"));
        assert!(caps.can_topic("public.events"));
        assert!(caps.can_topic("public.health"));

        // Non-public topics are denied
        assert!(!caps.can_topic("admin.secrets"));
        assert!(!caps.can_topic("mesh.internal"));
        assert!(!caps.can_topic("chain.append"));
        assert!(!caps.can_topic("governance.root"));

        // Enforced via CapabilityChecker as well
        let (checker, pid) = make_checker_with_entry(caps);
        assert!(checker.check_ipc_topic(pid, "public.status").is_ok());
        assert!(checker.check_ipc_topic(pid, "admin.secrets").is_err());
        assert!(checker.check_ipc_topic(pid, "mesh.peer.join").is_err());
    }

    #[test]
    fn restricted_scope_limits_topics_to_public() {
        let caps = AgentCapabilities {
            ipc_scope: IpcScope::Restricted(vec![1, 2]),
            ..Default::default()
        };
        assert!(caps.can_topic("public.feed"));
        assert!(!caps.can_topic("private.feed"));
        // Direct PID messaging still uses the allowlist
        assert!(caps.can_message(1));
        assert!(!caps.can_message(99));
    }

    #[test]
    fn is_shell_tool_recognizes_variants() {
        assert!(is_shell_tool("shell_exec"));
        assert!(is_shell_tool("exec_shell"));
        assert!(is_shell_tool("bash"));
        assert!(is_shell_tool("command"));
        assert!(is_shell_tool("run_command"));
        assert!(!is_shell_tool("read_file"));
        assert!(!is_shell_tool("web_search"));
    }

    // ── Browser elevation tests ────────────────────────────────────

    #[test]
    fn browser_elevation_needed_for_spawn() {
        let requested = AgentCapabilities {
            can_spawn: true,
            ..AgentCapabilities::browser_default()
        };
        assert!(AgentCapabilities::needs_elevation("browser", &requested));
    }

    #[test]
    fn browser_elevation_needed_for_network() {
        let requested = AgentCapabilities {
            can_network: true,
            ..AgentCapabilities::browser_default()
        };
        assert!(AgentCapabilities::needs_elevation("browser", &requested));
    }

    #[test]
    fn browser_elevation_not_needed_for_restricted() {
        // Browser defaults are already restricted -- no elevation needed.
        let requested = AgentCapabilities::browser_default();
        assert!(!AgentCapabilities::needs_elevation("browser", &requested));
    }

    #[test]
    fn non_browser_elevation_not_needed() {
        // Even with elevated caps, non-browser platform never needs elevation.
        let requested = AgentCapabilities {
            can_spawn: true,
            can_network: true,
            ipc_scope: IpcScope::All,
            ..Default::default()
        };
        assert!(!AgentCapabilities::needs_elevation("native", &requested));
        assert!(!AgentCapabilities::needs_elevation("wasi", &requested));
    }

    #[test]
    fn browser_elevation_needed_for_ipc_all() {
        let requested = AgentCapabilities {
            ipc_scope: IpcScope::All,
            ..AgentCapabilities::browser_default()
        };
        assert!(AgentCapabilities::needs_elevation("browser", &requested));
    }

    #[test]
    fn request_elevation_builds_request() {
        let current = AgentCapabilities::browser_default();
        let requested = AgentCapabilities {
            can_network: true,
            ..AgentCapabilities::browser_default()
        };
        let req = AgentCapabilities::request_elevation(&current, &requested, "browser");
        assert_eq!(req.pid, 0);
        assert!(!req.current.can_network);
        assert!(req.requested.can_network);
        assert!(req.reason.contains("browser"));
    }

    // ── K1-G4: Disk quota tests (os-patterns) ────────────────────

    #[cfg(feature = "os-patterns")]
    mod disk_quota_tests {
        use super::*;

        #[test]
        fn default_disk_quota_is_100_mib() {
            let limits = ResourceLimits::default();
            assert_eq!(limits.max_disk_bytes, 100 * 1024 * 1024);
        }

        #[test]
        fn browser_disk_quota_is_10_mib() {
            let caps = AgentCapabilities::browser_default();
            assert_eq!(caps.resource_limits.max_disk_bytes, 10 * 1024 * 1024);
        }

        #[test]
        fn disk_quota_serde_roundtrip() {
            let limits = ResourceLimits {
                max_disk_bytes: 50 * 1024 * 1024,
                ..Default::default()
            };
            let json = serde_json::to_string(&limits).unwrap();
            let restored: ResourceLimits = serde_json::from_str(&json).unwrap();
            assert_eq!(restored.max_disk_bytes, 50 * 1024 * 1024);
        }

        #[test]
        fn disk_quota_zero_means_unlimited() {
            let limits = ResourceLimits {
                max_disk_bytes: 0,
                ..Default::default()
            };
            assert_eq!(limits.max_disk_bytes, 0);
        }
    }
}
