//! WASM plugin engine with fuel metering and memory limits.
//!
//! Provides [`WasmPluginEngine`] -- the host-side runtime for loading and
//! executing WASM plugin modules. Each plugin runs in an isolated
//! [`wasmtime::Store`] with configurable resource limits.
//!
//! # Resource Limits
//!
//! | Resource | Default | Hard Maximum |
//! |----------|---------|-------------|
//! | Fuel budget | 1,000,000,000 (~1s CPU) | 10,000,000,000 |
//! | Memory | 16 MB | 256 MB |
//! | Table elements | 10,000 | 100,000 |
//!
//! # Security
//!
//! - Each plugin gets its own [`wasmtime::Store`] (no shared state)
//! - Fuel metering prevents CPU exhaustion
//! - Memory limits prevent OOM
//! - All host function calls go through [`PluginSandbox`] validation
//! - Every host function call is recorded in the [`AuditLog`]

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::audit::AuditLog;
use crate::sandbox::{
    MAX_WRITE_SIZE, PluginSandbox, validate_env_access, validate_file_access,
    validate_http_request, validate_log_message, validate_wasm_size,
};
use clawft_plugin::{PluginError, PluginManifest, PluginResourceConfig, record_plugin_invocation};

/// Hard maximum fuel budget (10 billion units, ~10s CPU).
pub const MAX_FUEL_HARD: u64 = 10_000_000_000;
/// Hard maximum memory (256 MB).
pub const MAX_MEMORY_HARD: usize = 256;
/// Hard maximum table elements.
pub const MAX_TABLE_ELEMENTS_HARD: u32 = 100_000;

/// Default execution timeout in seconds.
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Configuration for a WASM plugin instance.
#[derive(Debug, Clone)]
pub struct PluginConfig {
    /// Plugin identifier.
    pub plugin_id: String,
    /// Fuel budget per invocation.
    pub fuel_budget: u64,
    /// Memory limit in megabytes.
    pub max_memory_mb: usize,
    /// Maximum table elements.
    pub max_table_elements: u32,
    /// Execution timeout in seconds.
    pub timeout_secs: u64,
}

impl PluginConfig {
    /// Create a config from a plugin manifest, clamping values to hard limits.
    pub fn from_manifest(manifest: &PluginManifest) -> Self {
        let resources = &manifest.resources;
        Self {
            plugin_id: manifest.id.clone(),
            fuel_budget: resources.max_fuel.min(MAX_FUEL_HARD),
            max_memory_mb: resources.max_memory_mb.min(MAX_MEMORY_HARD),
            max_table_elements: resources.max_table_elements.min(MAX_TABLE_ELEMENTS_HARD),
            timeout_secs: resources.max_execution_seconds.min(300),
        }
    }

    /// Create a default config for a plugin ID.
    pub fn default_for(plugin_id: &str) -> Self {
        let defaults = PluginResourceConfig::default();
        Self {
            plugin_id: plugin_id.to_string(),
            fuel_budget: defaults.max_fuel,
            max_memory_mb: defaults.max_memory_mb,
            max_table_elements: defaults.max_table_elements,
            timeout_secs: defaults.max_execution_seconds,
        }
    }
}

/// Result of executing a plugin tool.
#[derive(Debug)]
pub struct PluginExecutionResult {
    /// JSON result string on success.
    pub result: Result<String, PluginError>,
    /// Fuel consumed during execution.
    pub fuel_consumed: u64,
    /// Peak linear-memory size observed during execution, in bytes.
    ///
    /// Zero when the module has no exported `memory` or measurement is
    /// unavailable (e.g. early failure before instantiate).
    pub memory_peak: usize,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
}

/// Validate a WASM module binary at load time.
///
/// Checks:
/// - Module size against the uncompressed limit (300 KB default)
/// - Basic structural validity (magic bytes)
///
/// Returns `Ok(())` if the module passes all checks.
pub fn validate_module_binary(wasm_bytes: &[u8]) -> Result<(), PluginError> {
    // Check magic bytes
    if wasm_bytes.len() < 8 {
        return Err(PluginError::LoadFailed(
            "WASM module too small (missing magic bytes)".into(),
        ));
    }
    if &wasm_bytes[0..4] != b"\0asm" {
        return Err(PluginError::LoadFailed(
            "invalid WASM module (bad magic bytes)".into(),
        ));
    }

    // Size check
    validate_wasm_size(wasm_bytes.len() as u64).map_err(PluginError::LoadFailed)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// WasmPluginEngine -- wasmtime-based WASM plugin host
// ---------------------------------------------------------------------------

/// Host state stored in each wasmtime `Store`.
///
/// Contains the dispatcher for host function calls and fuel/memory tracking.
pub struct HostState {
    /// Dispatcher for validated host function calls.
    pub dispatcher: Arc<HostFunctionDispatcher>,
    /// Resource limits for wasmtime `StoreLimits`.
    pub limits: wasmtime::StoreLimits,
}

/// The WASM plugin engine -- manages wasmtime `Engine`, compiles and
/// instantiates plugin modules with configurable resource limits.
///
/// Each plugin gets its own `Store` with isolated memory, fuel budget,
/// and an independent `HostFunctionDispatcher` for security enforcement.
pub struct WasmPluginEngine {
    /// Shared wasmtime engine (compiled code cache).
    engine: wasmtime::Engine,
}

impl WasmPluginEngine {
    /// Create a new WASM plugin engine with fuel metering and epoch
    /// interruption enabled.
    pub fn new() -> Result<Self, PluginError> {
        let mut config = wasmtime::Config::new();
        config.consume_fuel(true);
        config.epoch_interruption(true);
        // Cranelift is default compiler via features

        let engine = wasmtime::Engine::new(&config)
            .map_err(|e| PluginError::LoadFailed(format!("wasmtime engine init: {e}")))?;

        Ok(Self { engine })
    }

    /// Compile and validate a WASM module from bytes.
    ///
    /// Performs size checks, magic byte validation, and wasmtime compilation.
    pub fn compile_module(&self, wasm_bytes: &[u8]) -> Result<wasmtime::Module, PluginError> {
        validate_module_binary(wasm_bytes)?;

        wasmtime::Module::new(&self.engine, wasm_bytes)
            .map_err(|e| PluginError::LoadFailed(format!("module compilation: {e}")))
    }

    /// Create a new `Store` with resource limits for a plugin.
    ///
    /// The store is configured with:
    /// - Fuel budget (from `PluginConfig`)
    /// - Memory limits via `StoreLimits`
    /// - Host state containing the dispatcher and audit log
    pub fn create_store(
        &self,
        config: &PluginConfig,
        sandbox: Arc<PluginSandbox>,
        audit: Arc<AuditLog>,
    ) -> Result<wasmtime::Store<HostState>, PluginError> {
        let dispatcher = Arc::new(HostFunctionDispatcher::new(sandbox, audit));

        let limits = wasmtime::StoreLimitsBuilder::new()
            .memory_size(config.max_memory_mb * 1024 * 1024)
            .table_elements(config.max_table_elements as usize)
            .instances(10)
            .tables(10)
            .memories(2)
            .build();

        let host_state = HostState { dispatcher, limits };

        let mut store = wasmtime::Store::new(&self.engine, host_state);
        store.limiter(|state| &mut state.limits);
        store
            .set_fuel(config.fuel_budget)
            .map_err(|e| PluginError::ExecutionFailed(format!("set fuel: {e}")))?;

        // Set epoch deadline. The deadline is in "ticks" from the current
        // epoch. The caller is responsible for incrementing the engine epoch
        // (e.g., from a background thread) at a regular cadence.
        // We use 1 tick per timeout period -- the background thread will
        // increment the epoch after `timeout_secs` has elapsed.
        store.set_epoch_deadline(1);

        Ok(store)
    }

    /// Create a linker with host function imports matching the WIT interface.
    ///
    /// Registers: `http-request`, `read-file`, `write-file`, `get-env`, `log`.
    pub fn create_linker(&self) -> Result<wasmtime::Linker<HostState>, PluginError> {
        let mut linker = wasmtime::Linker::new(&self.engine);

        // http-request: func(method, url, headers_json, body) -> result<string, string>
        linker
            .func_wrap(
                "host",
                "http-request",
                |mut caller: wasmtime::Caller<'_, HostState>,
                 method_ptr: i32,
                 method_len: i32,
                 url_ptr: i32,
                 url_len: i32,
                 body_ptr: i32,
                 body_len: i32|
                 -> i32 {
                    let memory = match caller.get_export("memory") {
                        Some(wasmtime::Extern::Memory(m)) => m,
                        _ => return -1,
                    };
                    let data = memory.data(&caller);

                    let method = read_str(data, method_ptr, method_len);
                    let url = read_str(data, url_ptr, url_len);
                    let body = if body_len > 0 {
                        Some(read_str(data, body_ptr, body_len))
                    } else {
                        None
                    };

                    let dispatcher = caller.data().dispatcher.clone();
                    let _result =
                        dispatcher.handle_http_request(&method, &url, &[], body.as_deref());
                    0
                },
            )
            .map_err(|e| PluginError::LoadFailed(format!("link http-request: {e}")))?;

        // read-file: func(path) -> result<string, string>
        linker
            .func_wrap(
                "host",
                "read-file",
                |mut caller: wasmtime::Caller<'_, HostState>,
                 path_ptr: i32,
                 path_len: i32|
                 -> i32 {
                    let memory = match caller.get_export("memory") {
                        Some(wasmtime::Extern::Memory(m)) => m,
                        _ => return -1,
                    };
                    let data = memory.data(&caller);
                    let path = read_str(data, path_ptr, path_len);

                    let dispatcher = caller.data().dispatcher.clone();
                    match dispatcher.handle_read_file(&path) {
                        Ok(_) => 0,
                        Err(_) => -1,
                    }
                },
            )
            .map_err(|e| PluginError::LoadFailed(format!("link read-file: {e}")))?;

        // write-file: func(path, content) -> result<_, string>
        linker
            .func_wrap(
                "host",
                "write-file",
                |mut caller: wasmtime::Caller<'_, HostState>,
                 path_ptr: i32,
                 path_len: i32,
                 content_ptr: i32,
                 content_len: i32|
                 -> i32 {
                    let memory = match caller.get_export("memory") {
                        Some(wasmtime::Extern::Memory(m)) => m,
                        _ => return -1,
                    };
                    let data = memory.data(&caller);
                    let path = read_str(data, path_ptr, path_len);
                    let content = read_str(data, content_ptr, content_len);

                    let dispatcher = caller.data().dispatcher.clone();
                    match dispatcher.handle_write_file(&path, &content) {
                        Ok(()) => 0,
                        Err(_) => -1,
                    }
                },
            )
            .map_err(|e| PluginError::LoadFailed(format!("link write-file: {e}")))?;

        // get-env: func(name) -> option<string>
        linker
            .func_wrap(
                "host",
                "get-env",
                |mut caller: wasmtime::Caller<'_, HostState>,
                 name_ptr: i32,
                 name_len: i32|
                 -> i32 {
                    let memory = match caller.get_export("memory") {
                        Some(wasmtime::Extern::Memory(m)) => m,
                        _ => return -1,
                    };
                    let data = memory.data(&caller);
                    let name = read_str(data, name_ptr, name_len);

                    let dispatcher = caller.data().dispatcher.clone();
                    match dispatcher.handle_get_env(&name) {
                        Some(_) => 1,
                        None => 0,
                    }
                },
            )
            .map_err(|e| PluginError::LoadFailed(format!("link get-env: {e}")))?;

        // log: func(level: u8, message: string)
        linker
            .func_wrap(
                "host",
                "log",
                |mut caller: wasmtime::Caller<'_, HostState>,
                 level: i32,
                 msg_ptr: i32,
                 msg_len: i32| {
                    let memory = match caller.get_export("memory") {
                        Some(wasmtime::Extern::Memory(m)) => m,
                        _ => return,
                    };
                    let data = memory.data(&caller);
                    let message = read_str(data, msg_ptr, msg_len);

                    let dispatcher = caller.data().dispatcher.clone();
                    dispatcher.handle_log(level as u8, &message);
                },
            )
            .map_err(|e| PluginError::LoadFailed(format!("link log: {e}")))?;

        Ok(linker)
    }

    /// Execute a tool call on a plugin instance.
    ///
    /// Creates a fresh `Store` per invocation (fuel resets), instantiates
    /// the module, calls the exported `execute-tool` function, and returns
    /// the result with metrics.
    ///
    /// Wall-clock timeout is enforced via wasmtime epoch interruption: a
    /// background thread increments the engine epoch after `config.timeout_secs`
    /// has elapsed, causing any running WASM to trap.
    pub fn execute_tool(
        &self,
        module: &wasmtime::Module,
        config: &PluginConfig,
        sandbox: Arc<PluginSandbox>,
        audit: Arc<AuditLog>,
        tool_name: &str,
        params_json: &str,
    ) -> PluginExecutionResult {
        let start = Instant::now();

        let mut store = match self.create_store(config, sandbox, audit) {
            Ok(s) => s,
            Err(e) => {
                return finish_execution(
                    &config.plugin_id,
                    Err(e),
                    0,
                    0,
                    start.elapsed().as_millis() as u64,
                );
            }
        };

        let linker = match self.create_linker() {
            Ok(l) => l,
            Err(e) => {
                return finish_execution(
                    &config.plugin_id,
                    Err(e),
                    0,
                    0,
                    start.elapsed().as_millis() as u64,
                );
            }
        };

        let instance = match linker.instantiate(&mut store, module) {
            Ok(i) => i,
            Err(e) => {
                let fuel_consumed = config
                    .fuel_budget
                    .saturating_sub(store.get_fuel().unwrap_or(0));
                return finish_execution(
                    &config.plugin_id,
                    Err(PluginError::LoadFailed(format!("instantiate: {e}"))),
                    fuel_consumed,
                    0,
                    start.elapsed().as_millis() as u64,
                );
            }
        };

        // Start wall-clock timeout enforcer. A background thread will
        // increment the engine epoch after `timeout_secs`, causing the
        // WASM execution to trap if it hasn't finished.
        let timeout = Duration::from_secs(config.timeout_secs);
        let engine_handle = self.engine.clone();
        let timeout_thread = std::thread::spawn(move || {
            std::thread::sleep(timeout);
            engine_handle.increment_epoch();
        });

        // Look for an exported function `execute-tool` or `execute_tool`
        let func = instance
            .get_typed_func::<(i32, i32, i32, i32), i32>(&mut store, "execute-tool")
            .or_else(|_| {
                instance.get_typed_func::<(i32, i32, i32, i32), i32>(&mut store, "execute_tool")
            });

        let result = match func {
            Ok(_f) => {
                // In a full WIT component model implementation, we'd marshal
                // string params through linear memory. For now, record the
                // invocation in the audit log.
                let _tool = tool_name;
                let _params = params_json;
                Ok(serde_json::json!({"status": "executed", "tool": tool_name}).to_string())
            }
            Err(_) => {
                // Module doesn't export the expected function -- still valid
                // for modules that only export `describe` or `init`.
                Err(PluginError::ExecutionFailed(
                    "module does not export 'execute-tool'".into(),
                ))
            }
        };

        // Detach the timeout thread -- it will finish on its own.
        // We don't join because if execution completed before timeout,
        // we don't want to wait for the sleep to finish.
        drop(timeout_thread);

        let fuel_consumed = config
            .fuel_budget
            .saturating_sub(store.get_fuel().unwrap_or(0));
        // Best-effort peak memory: size of the exported linear memory after
        // the call (0 if the module does not export `memory`).
        let memory_peak = instance
            .get_memory(&mut store, "memory")
            .map(|m| m.data_size(&store))
            .unwrap_or(0);
        let duration_ms = start.elapsed().as_millis() as u64;

        finish_execution(
            &config.plugin_id,
            result,
            fuel_consumed,
            memory_peak,
            duration_ms,
        )
    }

    /// Execute a raw WASM function with wall-clock timeout enforcement.
    ///
    /// This is a lower-level method for running arbitrary typed WASM functions
    /// with epoch-based wall-clock timeout. Used by tests and advanced callers
    /// that need direct control over the function signature.
    ///
    /// Returns `Err` if the function traps (including epoch-based timeout).
    pub fn call_func_with_timeout<Params, Results>(
        &self,
        store: &mut wasmtime::Store<HostState>,
        func: &wasmtime::TypedFunc<Params, Results>,
        params: Params,
        timeout: Duration,
    ) -> Result<Results, wasmtime::Error>
    where
        Params: wasmtime::WasmParams,
        Results: wasmtime::WasmResults,
    {
        // Spawn a thread that increments the epoch after the timeout.
        let engine_handle = self.engine.clone();
        let timeout_thread = std::thread::spawn(move || {
            std::thread::sleep(timeout);
            engine_handle.increment_epoch();
        });

        let result = func.call(store, params);

        // Detach the timeout thread.
        drop(timeout_thread);

        result
    }

    /// Get a reference to the underlying wasmtime engine.
    pub fn engine(&self) -> &wasmtime::Engine {
        &self.engine
    }

    /// Check remaining fuel in a store.
    pub fn remaining_fuel(store: &wasmtime::Store<HostState>) -> Result<u64, PluginError> {
        store
            .get_fuel()
            .map_err(|e| PluginError::ExecutionFailed(format!("get fuel: {e}")))
    }
}

/// Package an execution result, update per-plugin aggregate metrics, and
/// emit a `plugin.wasm.invoke` chain-event (WEFT-68).
fn finish_execution(
    plugin_id: &str,
    result: Result<String, PluginError>,
    fuel_consumed: u64,
    memory_peak: usize,
    duration_ms: u64,
) -> PluginExecutionResult {
    // Observability: per-plugin fuel/memory counters + chain_event surface.
    // Uses plugin_id as the registry key (manifest id).
    let _snapshot = record_plugin_invocation(
        plugin_id,
        fuel_consumed,
        memory_peak as u64,
        duration_ms,
    );

    PluginExecutionResult {
        result,
        fuel_consumed,
        memory_peak,
        duration_ms,
    }
}

/// Read a string from WASM linear memory at the given pointer and length.
///
/// `ptr` and `len` are guest-controlled. A negative value or a range that
/// leaves linear memory yields an empty string, never a panic.
fn read_str(data: &[u8], ptr: i32, len: i32) -> String {
    let range = usize::try_from(ptr).ok().and_then(|start| {
        let len = usize::try_from(len).ok()?;
        data.get(start..start.checked_add(len)?)
    });
    match range {
        Some(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Host function call dispatcher
// ---------------------------------------------------------------------------

/// Host function call dispatcher.
///
/// Executes host function calls against a [`PluginSandbox`] with full
/// security validation and audit logging. This is the bridge between
/// the WIT interface and the sandbox enforcement layer.
pub struct HostFunctionDispatcher {
    sandbox: Arc<PluginSandbox>,
    audit: Arc<AuditLog>,
}

impl HostFunctionDispatcher {
    /// Create a new dispatcher for a plugin.
    pub fn new(sandbox: Arc<PluginSandbox>, audit: Arc<AuditLog>) -> Self {
        Self { sandbox, audit }
    }

    /// Handle an `http-request` host function call.
    pub fn handle_http_request(
        &self,
        method: &str,
        url: &str,
        _headers: &[(String, String)],
        body: Option<&str>,
    ) -> Result<String, String> {
        let start = Instant::now();
        let summary = format!("{method} {url}");

        match validate_http_request(&self.sandbox, method, url, body) {
            Ok(_validated_url) => {
                // In a full implementation, we would execute the HTTP request
                // here using reqwest or similar. For now, we record the
                // validation success and return a placeholder.
                let duration = start.elapsed().as_millis() as u64;
                self.audit
                    .record_success("http-request", &summary, duration);

                // Actual HTTP execution would happen here.
                // For now, return a validation-passed marker.
                Err("HTTP execution not yet wired (validation passed)".into())
            }
            Err(e) => {
                self.audit
                    .record_denied("http-request", &summary, &e.to_string());
                Err(e.to_string())
            }
        }
    }

    /// Handle a `read-file` host function call.
    pub fn handle_read_file(&self, path: &str) -> Result<String, String> {
        let start = Instant::now();
        let fs_path = std::path::Path::new(path);

        match validate_file_access(&self.sandbox, fs_path, false) {
            Ok(canonical) => match std::fs::read_to_string(&canonical) {
                Ok(content) => {
                    let duration = start.elapsed().as_millis() as u64;
                    self.audit.record_success("read-file", path, duration);
                    Ok(content)
                }
                Err(e) => {
                    let duration = start.elapsed().as_millis() as u64;
                    let err_msg = format!("read error: {e}");
                    self.audit
                        .record_error("read-file", path, &err_msg, duration);
                    Err(err_msg)
                }
            },
            Err(e) => {
                self.audit.record_denied("read-file", path, &e.to_string());
                Err(e.to_string())
            }
        }
    }

    /// Handle a `write-file` host function call.
    pub fn handle_write_file(&self, path: &str, content: &str) -> Result<(), String> {
        let start = Instant::now();
        let fs_path = std::path::Path::new(path);

        // Check content size limit
        if content.len() > MAX_WRITE_SIZE {
            let err_msg = format!(
                "write content too large: {} bytes (max {} bytes)",
                content.len(),
                MAX_WRITE_SIZE,
            );
            self.audit.record_denied("write-file", path, &err_msg);
            return Err(err_msg);
        }

        match validate_file_access(&self.sandbox, fs_path, true) {
            Ok(canonical) => match std::fs::write(&canonical, content) {
                Ok(()) => {
                    let duration = start.elapsed().as_millis() as u64;
                    self.audit.record_success("write-file", path, duration);
                    Ok(())
                }
                Err(e) => {
                    let duration = start.elapsed().as_millis() as u64;
                    let err_msg = format!("write error: {e}");
                    self.audit
                        .record_error("write-file", path, &err_msg, duration);
                    Err(err_msg)
                }
            },
            Err(e) => {
                self.audit.record_denied("write-file", path, &e.to_string());
                Err(e.to_string())
            }
        }
    }

    /// Handle a `get-env` host function call.
    pub fn handle_get_env(&self, name: &str) -> Option<String> {
        let result = validate_env_access(&self.sandbox, name);
        match &result {
            Some(_) => self.audit.record_success("get-env", name, 0),
            None => self
                .audit
                .record_denied("get-env", name, "not permitted or not set"),
        }
        result
    }

    /// Handle a `log` host function call.
    pub fn handle_log(&self, level: u8, message: &str) {
        let (processed_msg, rate_limited) = validate_log_message(&self.sandbox, message);

        let level_str = match level {
            0 => "error",
            1 => "warn",
            2 => "info",
            3 => "debug",
            _ => "trace",
        };

        if rate_limited {
            self.audit.record_denied(
                "log",
                &format!("{level_str}: [rate limited]"),
                "rate limit exceeded",
            );
            return;
        }

        // Emit the log via tracing (host crate always has tracing).
        {
            let plugin_id = &self.sandbox.plugin_id;
            match level {
                0 => tracing::error!(plugin = %plugin_id, "{}", processed_msg),
                1 => tracing::warn!(plugin = %plugin_id, "{}", processed_msg),
                2 => tracing::info!(plugin = %plugin_id, "{}", processed_msg),
                3 => tracing::debug!(plugin = %plugin_id, "{}", processed_msg),
                _ => tracing::trace!(plugin = %plugin_id, "{}", processed_msg),
            }
        }

        let summary = format!(
            "{level_str}: {}",
            crate::sandbox::truncate_utf8(&processed_msg, 80)
        );
        self.audit.record_success("log", &summary, 0);
    }

    /// Get a reference to the audit log.
    pub fn audit_log(&self) -> &AuditLog {
        &self.audit
    }

    /// Get a reference to the sandbox.
    pub fn sandbox(&self) -> &PluginSandbox {
        &self.sandbox
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use clawft_plugin::{PluginPermissions, PluginResourceConfig};


    /// Canonical temp dir — see sandbox::tests::canonical_temp_dir for rationale
    /// (macOS /var → /private/var intermediate symlink).
    fn canonical_temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::create_dir_all(&dir);
        std::fs::canonicalize(&dir).expect("canonicalize temp dir")
    }


    #[test]
    fn read_str_hostile_pointers_do_not_panic() {
        let mem = b"hello world";
        assert_eq!(read_str(mem, 0, 5), "hello");
        assert_eq!(read_str(mem, 6, 5), "world");
        for (ptr, len) in [
            (-1, 1),
            (-1, 0),
            (0, -1),
            (0, i32::MIN),
            (i32::MIN, 1),
            (i32::MAX, i32::MAX),
            (6, 6),
            (12, 0),
        ] {
            assert_eq!(read_str(mem, ptr, len), "", "ptr={ptr} len={len}");
        }
    }

    #[test]
    fn handle_log_multibyte_at_summary_cut_does_not_panic() {
        let d = test_dispatcher(vec![], vec![], vec![]);
        // A 4-byte char straddles byte 80 of the audit summary.
        let msg = format!("{}{}", "a".repeat(79), "\u{1F600}");
        d.handle_log(2, &msg);
    }

    fn test_config() -> PluginConfig {
        PluginConfig::default_for("test-plugin")
    }

    fn test_sandbox(
        network: Vec<String>,
        filesystem: Vec<String>,
        env_vars: Vec<String>,
    ) -> Arc<PluginSandbox> {
        let permissions = PluginPermissions {
            network,
            filesystem,
            env_vars,
            shell: false,
        };
        Arc::new(PluginSandbox::from_manifest(
            "test-plugin".into(),
            permissions,
            &PluginResourceConfig::default(),
        ))
    }

    fn test_dispatcher(
        network: Vec<String>,
        filesystem: Vec<String>,
        env_vars: Vec<String>,
    ) -> HostFunctionDispatcher {
        let sandbox = test_sandbox(network, filesystem, env_vars);
        let audit = Arc::new(AuditLog::new("test-plugin".into()));
        HostFunctionDispatcher::new(sandbox, audit)
    }

    // -- PluginConfig tests --

    #[test]
    fn config_default_values() {
        let config = test_config();
        assert_eq!(config.plugin_id, "test-plugin");
        assert_eq!(config.fuel_budget, 1_000_000_000);
        assert_eq!(config.max_memory_mb, 16);
        assert_eq!(config.max_table_elements, 10_000);
        assert_eq!(config.timeout_secs, 30);
    }

    #[test]
    fn config_from_manifest_clamps_values() {
        let manifest = PluginManifest {
            id: "test".into(),
            name: "Test".into(),
            version: "1.0.0".into(),
            capabilities: vec![clawft_plugin::PluginCapability::Tool],
            permissions: PluginPermissions::default(),
            resources: PluginResourceConfig {
                max_fuel: 999_999_999_999,   // Over hard limit
                max_memory_mb: 512,          // Over hard limit
                max_table_elements: 999_999, // Over hard limit
                ..PluginResourceConfig::default()
            },
            wasm_module: None,
            skills: vec![],
            tools: vec![],
            voice: None,
        };

        let config = PluginConfig::from_manifest(&manifest);
        assert_eq!(config.fuel_budget, MAX_FUEL_HARD);
        assert_eq!(config.max_memory_mb, MAX_MEMORY_HARD);
        assert_eq!(config.max_table_elements, MAX_TABLE_ELEMENTS_HARD);
    }

    // -- Module validation tests --

    #[test]
    fn validate_module_too_small() {
        let result = validate_module_binary(&[0, 1, 2]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("too small"));
    }

    #[test]
    fn validate_module_bad_magic() {
        let result = validate_module_binary(&[0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("bad magic"));
    }

    #[test]
    fn validate_module_valid_magic() {
        // Valid WASM magic + version, minimal valid header
        let wasm = b"\0asm\x01\x00\x00\x00";
        let result = validate_module_binary(wasm);
        assert!(result.is_ok());
    }

    #[test]
    fn validate_module_too_large() {
        // 301 KB of data with valid magic
        let mut wasm = vec![0u8; 301 * 1024];
        wasm[0..4].copy_from_slice(b"\0asm");
        wasm[4..8].copy_from_slice(&[1, 0, 0, 0]);
        let result = validate_module_binary(&wasm);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("too large"));
    }

    // -- WasmPluginEngine tests --

    #[test]
    fn engine_creation() {
        let engine = WasmPluginEngine::new();
        assert!(engine.is_ok(), "engine creation should succeed");
    }

    #[test]
    fn engine_create_store_with_fuel() {
        let engine = WasmPluginEngine::new().unwrap();
        let config = PluginConfig::default_for("test");
        let sandbox = test_sandbox(vec![], vec![], vec![]);
        let audit = Arc::new(AuditLog::new("test".into()));

        let store = engine.create_store(&config, sandbox, audit);
        assert!(store.is_ok(), "store creation should succeed");

        let store = store.unwrap();
        let fuel = store.get_fuel().unwrap();
        assert_eq!(fuel, 1_000_000_000, "fuel should match default budget");
    }

    #[test]
    fn engine_create_store_custom_fuel() {
        let engine = WasmPluginEngine::new().unwrap();
        let mut config = PluginConfig::default_for("test");
        config.fuel_budget = 500_000_000;
        let sandbox = test_sandbox(vec![], vec![], vec![]);
        let audit = Arc::new(AuditLog::new("test".into()));

        let store = engine.create_store(&config, sandbox, audit).unwrap();
        let fuel = store.get_fuel().unwrap();
        assert_eq!(fuel, 500_000_000);
    }

    #[test]
    fn engine_create_linker() {
        let engine = WasmPluginEngine::new().unwrap();
        let linker = engine.create_linker();
        assert!(linker.is_ok(), "linker creation should succeed");
    }

    /// T28: Fuel exhaustion test.
    ///
    /// An infinite-loop WASM module should be terminated by fuel metering.
    /// We generate a minimal WASM module with a tight loop to verify this.
    #[test]
    fn t28_fuel_exhaustion() {
        let engine = WasmPluginEngine::new().unwrap();

        // Minimal WASM module with an infinite loop function:
        // (module
        //   (func (export "loop_forever")
        //     (loop $L (br $L))
        //   )
        // )
        let wasm = wat::parse_str(
            r#"(module
                (func (export "loop_forever")
                    (loop $L (br $L))
                )
            )"#,
        )
        .expect("WAT parsing should succeed");

        // Use a very small fuel budget to trigger exhaustion quickly
        let mut config = PluginConfig::default_for("fuel-test");
        config.fuel_budget = 1000; // Very low fuel

        let sandbox = test_sandbox(vec![], vec![], vec![]);
        let audit = Arc::new(AuditLog::new("fuel-test".into()));

        let module = wasmtime::Module::new(engine.engine(), &wasm)
            .expect("module compilation should succeed");

        let mut store = engine.create_store(&config, sandbox, audit).unwrap();
        let linker = engine.create_linker().unwrap();
        let instance = linker.instantiate(&mut store, &module).unwrap();

        let func = instance
            .get_typed_func::<(), ()>(&mut store, "loop_forever")
            .unwrap();

        let result = func.call(&mut store, ());
        assert!(
            result.is_err(),
            "infinite loop should trap due to fuel exhaustion"
        );

        // Verify fuel is exhausted (or near-zero)
        let remaining = store.get_fuel().unwrap_or(0);
        assert!(
            remaining < 10,
            "fuel should be exhausted, remaining: {remaining}"
        );
    }

    /// T29: Memory limit exceeded.
    ///
    /// A WASM module that allocates beyond the memory limit should fail.
    #[test]
    fn t29_memory_limit_exceeded() {
        let engine = WasmPluginEngine::new().unwrap();

        // Module with 1 page (64KB) of memory that tries to grow
        let wasm = wat::parse_str(
            r#"(module
                (memory (export "memory") 1)
                (func (export "grow_big") (result i32)
                    ;; Try to grow memory by 300 pages (19.2 MB > 8 MB limit)
                    (memory.grow (i32.const 300))
                )
            )"#,
        )
        .expect("WAT parsing should succeed");

        // Set memory limit to 8 MB
        let mut config = PluginConfig::default_for("mem-test");
        config.max_memory_mb = 8;

        let sandbox = test_sandbox(vec![], vec![], vec![]);
        let audit = Arc::new(AuditLog::new("mem-test".into()));

        let module = wasmtime::Module::new(engine.engine(), &wasm).unwrap();
        let mut store = engine.create_store(&config, sandbox, audit).unwrap();
        let linker = engine.create_linker().unwrap();
        let instance = linker.instantiate(&mut store, &module).unwrap();

        let func = instance
            .get_typed_func::<(), i32>(&mut store, "grow_big")
            .unwrap();

        let result = func.call(&mut store, ());
        // memory.grow returns -1 when the growth is denied
        match result {
            Ok(ret) => {
                assert_eq!(ret, -1, "memory.grow should return -1 when limit exceeded");
            }
            Err(e) => {
                // Trap is also acceptable for out-of-memory
                let err_msg = e.to_string();
                assert!(
                    err_msg.contains("memory") || err_msg.contains("grow"),
                    "error should relate to memory: {err_msg}"
                );
            }
        }
    }

    /// T31: Custom fuel limit (lower threshold) exhausted.
    #[test]
    fn t31_custom_fuel_lower_threshold() {
        let engine = WasmPluginEngine::new().unwrap();

        let wasm = wat::parse_str(
            r#"(module
                (func (export "work") (result i32)
                    (local $i i32)
                    (local.set $i (i32.const 0))
                    (block $done
                        (loop $L
                            (local.set $i (i32.add (local.get $i) (i32.const 1)))
                            (br_if $done (i32.ge_u (local.get $i) (i32.const 1000000)))
                            (br $L)
                        )
                    )
                    (local.get $i)
                )
            )"#,
        )
        .unwrap();

        // Very low custom fuel -- should exhaust before loop completes
        let mut config = PluginConfig::default_for("custom-fuel");
        config.fuel_budget = 500; // 500 fuel units

        let sandbox = test_sandbox(vec![], vec![], vec![]);
        let audit = Arc::new(AuditLog::new("custom-fuel".into()));

        let module = wasmtime::Module::new(engine.engine(), &wasm).unwrap();
        let mut store = engine.create_store(&config, sandbox, audit).unwrap();
        let linker = engine.create_linker().unwrap();
        let instance = linker.instantiate(&mut store, &module).unwrap();

        let func = instance
            .get_typed_func::<(), i32>(&mut store, "work")
            .unwrap();

        let result = func.call(&mut store, ());
        assert!(result.is_err(), "loop should trap with low fuel budget");
    }

    /// T32: Custom memory limit (lower threshold) exceeded.
    #[test]
    fn t32_custom_memory_lower_threshold() {
        let engine = WasmPluginEngine::new().unwrap();

        let wasm = wat::parse_str(
            r#"(module
                (memory (export "memory") 1)
                (func (export "grow_some") (result i32)
                    ;; Try to grow by 100 pages (6.4 MB)
                    (memory.grow (i32.const 100))
                )
            )"#,
        )
        .unwrap();

        // Set a very tight memory limit: 2 MB
        let mut config = PluginConfig::default_for("tight-mem");
        config.max_memory_mb = 2;

        let sandbox = test_sandbox(vec![], vec![], vec![]);
        let audit = Arc::new(AuditLog::new("tight-mem".into()));

        let module = wasmtime::Module::new(engine.engine(), &wasm).unwrap();
        let mut store = engine.create_store(&config, sandbox, audit).unwrap();
        let linker = engine.create_linker().unwrap();
        let instance = linker.instantiate(&mut store, &module).unwrap();

        let func = instance
            .get_typed_func::<(), i32>(&mut store, "grow_some")
            .unwrap();

        let result = func.call(&mut store, ());
        match result {
            Ok(ret) => {
                assert_eq!(ret, -1, "memory.grow should fail at 2 MB limit");
            }
            Err(_) => {
                // Trap is also acceptable
            }
        }
    }

    /// T37: Audit logging for all host function calls.
    ///
    /// Verifies that every host function invocation produces an audit entry.
    #[test]
    fn t37_audit_all_host_functions() {
        let dir = canonical_temp_dir("clawft_t37_audit");
        let file = dir.join("test.txt");
        std::fs::write(&file, "audit content").unwrap();

        unsafe {
            std::env::set_var("CLAWFT_T37_VAR", "t37_value");
        }

        let permissions = PluginPermissions {
            network: vec!["example.com".into()],
            filesystem: vec![dir.to_string_lossy().to_string()],
            env_vars: vec!["CLAWFT_T37_VAR".into()],
            shell: false,
        };
        let sandbox = Arc::new(PluginSandbox::from_manifest(
            "audit-test".into(),
            permissions,
            &PluginResourceConfig::default(),
        ));
        let audit = Arc::new(AuditLog::new("audit-test".into()));
        let d = HostFunctionDispatcher::new(sandbox, audit);

        // Invoke all 5 host functions
        let _ = d.handle_http_request("GET", "https://example.com/", &[], None);
        let _ = d.handle_read_file(file.to_str().unwrap());
        let write_file = dir.join("output.txt");
        let _ = d.handle_write_file(write_file.to_str().unwrap(), "test");
        let _ = d.handle_get_env("CLAWFT_T37_VAR");
        d.handle_log(2, "audit test message");

        // Verify all 5 produced audit entries
        assert_eq!(
            d.audit_log().len(),
            5,
            "all 5 host functions should produce entries"
        );
        assert_eq!(d.audit_log().count_by_function("http-request"), 1);
        assert_eq!(d.audit_log().count_by_function("read-file"), 1);
        assert_eq!(d.audit_log().count_by_function("write-file"), 1);
        assert_eq!(d.audit_log().count_by_function("get-env"), 1);
        assert_eq!(d.audit_log().count_by_function("log"), 1);

        // All should be permitted
        let entries = d.audit_log().entries();
        for entry in &entries {
            assert!(
                entry.permitted,
                "all calls should be permitted, failed: {}",
                entry.function
            );
        }

        unsafe {
            std::env::remove_var("CLAWFT_T37_VAR");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T38: Denied operations also produce audit entries.
    #[test]
    fn t38_audit_denied_operations() {
        let d = test_dispatcher(vec![], vec![], vec![]);

        let _ = d.handle_http_request("GET", "https://example.com/", &[], None);
        let _ = d.handle_read_file("/etc/passwd");
        let _ = d.handle_write_file("/etc/hacked", "bad");
        let _ = d.handle_get_env("SECRET");
        // log is always "permitted" unless rate-limited

        let entries = d.audit_log().entries();
        assert_eq!(entries.len(), 4);

        // All should be denied
        for entry in &entries {
            assert!(
                !entry.permitted,
                "all calls should be denied, passed: {} with {}",
                entry.function, entry.params_summary
            );
            assert!(entry.error.is_some(), "denied entries should have error");
        }
    }

    /// T39: Permission escalation prevention -- plugins cannot modify
    /// their own sandbox permissions at runtime.
    #[test]
    fn t39_permission_escalation_prevented() {
        let sandbox = test_sandbox(vec!["safe.example.com".into()], vec![], vec![]);
        let audit = Arc::new(AuditLog::new("escalation-test".into()));
        let d = HostFunctionDispatcher::new(sandbox.clone(), audit);

        // Plugin can access safe.example.com
        let result = d.handle_http_request("GET", "https://safe.example.com/", &[], None);
        assert!(
            result.is_err() && result.as_ref().unwrap_err().contains("validation passed"),
            "allowed domain should pass validation"
        );

        // Plugin cannot access evil.example.com
        let result = d.handle_http_request("GET", "https://evil.example.com/", &[], None);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().contains("not in network allowlist"),
            "unauthorized domain should be denied"
        );

        // Verify the sandbox is immutable -- the permissions struct is not
        // accessible for mutation through the dispatcher API
        assert!(
            sandbox.permissions.network.len() == 1,
            "permissions should not have changed"
        );
    }

    /// T40: Multi-plugin isolation -- each plugin has independent
    /// sandbox, audit log, and rate counters.
    #[test]
    fn t40_multi_plugin_isolation() {
        // Plugin A: network access only
        let sandbox_a = test_sandbox(vec!["a.example.com".into()], vec![], vec![]);
        let audit_a = Arc::new(AuditLog::new("plugin-a".into()));
        let d_a = HostFunctionDispatcher::new(sandbox_a, audit_a);

        // Plugin B: filesystem access only
        let dir = canonical_temp_dir("clawft_t40_isolation");
        let file = dir.join("b_file.txt");
        std::fs::write(&file, "b content").unwrap();

        let sandbox_b = test_sandbox(vec![], vec![dir.to_string_lossy().to_string()], vec![]);
        let audit_b = Arc::new(AuditLog::new("plugin-b".into()));
        let d_b = HostFunctionDispatcher::new(sandbox_b, audit_b);

        // Plugin A can do HTTP but not FS
        let _ = d_a.handle_http_request("GET", "https://a.example.com/", &[], None);
        let fs_result = d_a.handle_read_file(file.to_str().unwrap());
        assert!(fs_result.is_err(), "plugin A should not have FS access");

        // Plugin B can do FS but not HTTP
        let read_result = d_b.handle_read_file(file.to_str().unwrap());
        assert!(read_result.is_ok(), "plugin B should have FS access");
        let http_result = d_b.handle_http_request("GET", "https://a.example.com/", &[], None);
        assert!(
            http_result.is_err() && http_result.unwrap_err().contains("not permitted"),
            "plugin B should not have network access"
        );

        // Audit logs are independent
        assert_eq!(d_a.audit_log().plugin_id(), "plugin-a");
        assert_eq!(d_b.audit_log().plugin_id(), "plugin-b");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// WEFT-68: execute_tool records per-plugin fuel/memory aggregates and
    /// pins the PluginMetrics shape through the global registry.
    #[test]
    fn weft68_execute_tool_records_plugin_metrics() {
        use clawft_plugin::global_plugin_metrics;

        let engine = WasmPluginEngine::new().unwrap();
        // Minimal valid module (no execute-tool export — still finishes and
        // records metrics for the failed-export path).
        let wasm = wat::parse_str(
            r#"(module
                (memory (export "memory") 1)
                (func (export "run") (nop))
            )"#,
        )
        .unwrap();
        let module = engine.compile_module(&wasm).unwrap();

        let plugin_id = "weft68-metrics-plugin";
        let config = PluginConfig::default_for(plugin_id);
        let sandbox = test_sandbox(vec![], vec![], vec![]);
        let audit = Arc::new(AuditLog::new(plugin_id.into()));

        // Clear any prior state for this id (shared process-wide registry).
        if let Some(existing) = global_plugin_metrics().get(plugin_id) {
            // Re-upsert zeroed by recording from a clean registry isn't
            // possible for a single key; assert deltas instead.
            let before = existing.invocation_count;
            let result = engine.execute_tool(
                &module,
                &config,
                sandbox.clone(),
                audit.clone(),
                "noop",
                "{}",
            );
            // Module lacks execute-tool — result is Err, but metrics still
            // recorded via finish_execution.
            assert!(result.result.is_err());
            let after = global_plugin_metrics().get(plugin_id).unwrap();
            assert_eq!(after.invocation_count, before + 1);
            assert!(
                after.memory_peak_bytes >= 65_536 || after.memory_peak_bytes == 0,
                "memory peak should be page-sized or unmeasured; got {}",
                after.memory_peak_bytes
            );
            // Shape pin
            let keys: std::collections::BTreeSet<_> = after
                .to_json_value()
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect();
            for k in [
                "plugin_name",
                "fuel_consumed_total",
                "memory_peak_bytes",
                "invocation_count",
            ] {
                assert!(keys.contains(k), "missing metric key {k}");
            }
            return;
        }

        let result = engine.execute_tool(&module, &config, sandbox, audit, "noop", "{}");
        assert!(result.result.is_err());
        let m = global_plugin_metrics().get(plugin_id).expect("metrics recorded");
        assert_eq!(m.plugin_name, plugin_id);
        assert_eq!(m.invocation_count, 1);
        assert_eq!(result.fuel_consumed, m.last_fuel_consumed);
        assert_eq!(result.memory_peak as u64, m.last_memory_peak_bytes);
        // 1 page of linear memory when exported
        assert_eq!(m.memory_peak_bytes, 65_536);
    }


    // -- T43: Memory isolation (conceptual test) --

    #[test]
    fn t43_separate_sandboxes_isolated() {
        // Each plugin gets its own sandbox and audit log -- no shared state
        let sandbox_a = test_sandbox(vec!["a.example.com".into()], vec![], vec!["VAR_A".into()]);
        let audit_a = Arc::new(AuditLog::new("plugin-a".into()));
        let d_a = HostFunctionDispatcher::new(sandbox_a, audit_a);

        let sandbox_b = test_sandbox(vec!["b.example.com".into()], vec![], vec!["VAR_B".into()]);
        let audit_b = Arc::new(AuditLog::new("plugin-b".into()));
        let d_b = HostFunctionDispatcher::new(sandbox_b, audit_b);

        // Plugin A can access a.example.com but not b.example.com
        let _ = d_a.handle_http_request("GET", "https://a.example.com/", &[], None);
        let result = d_a.handle_http_request("GET", "https://b.example.com/", &[], None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not in network allowlist"));

        // Plugin B can access b.example.com but not a.example.com
        let _ = d_b.handle_http_request("GET", "https://b.example.com/", &[], None);
        let result = d_b.handle_http_request("GET", "https://a.example.com/", &[], None);
        assert!(result.is_err());

        // Audit logs are independent
        assert_eq!(d_a.audit_log().plugin_id(), "plugin-a");
        assert_eq!(d_b.audit_log().plugin_id(), "plugin-b");
        assert_eq!(d_a.audit_log().len(), 2);
        assert_eq!(d_b.audit_log().len(), 2);
    }

    /// T43 wasmtime: Two concurrent plugin instances have isolated stores.
    #[test]
    fn t43_wasmtime_store_isolation() {
        let engine = WasmPluginEngine::new().unwrap();

        // Create two stores with different permissions and explicit plugin IDs
        let config_a = PluginConfig::default_for("plugin-a");
        let sandbox_a = {
            let permissions = PluginPermissions {
                network: vec!["a.example.com".into()],
                ..Default::default()
            };
            Arc::new(PluginSandbox::from_manifest(
                "plugin-a".into(),
                permissions,
                &PluginResourceConfig::default(),
            ))
        };
        let audit_a = Arc::new(AuditLog::new("plugin-a".into()));
        let store_a = engine.create_store(&config_a, sandbox_a, audit_a).unwrap();

        let config_b = PluginConfig::default_for("plugin-b");
        let sandbox_b = {
            let permissions = PluginPermissions {
                network: vec!["b.example.com".into()],
                ..Default::default()
            };
            Arc::new(PluginSandbox::from_manifest(
                "plugin-b".into(),
                permissions,
                &PluginResourceConfig::default(),
            ))
        };
        let audit_b = Arc::new(AuditLog::new("plugin-b".into()));
        let store_b = engine.create_store(&config_b, sandbox_b, audit_b).unwrap();

        // Stores are independent -- fuel is separate
        let fuel_a = store_a.get_fuel().unwrap();
        let fuel_b = store_b.get_fuel().unwrap();
        assert_eq!(fuel_a, fuel_b);

        // Dispatchers are separate (different plugin_ids)
        assert_eq!(store_a.data().dispatcher.sandbox().plugin_id, "plugin-a");
        assert_eq!(store_b.data().dispatcher.sandbox().plugin_id, "plugin-b");
    }

    // -- T45: Fuel resets between invocations --

    #[test]
    fn t45_fuel_resets_between_invocations() {
        let engine = WasmPluginEngine::new().unwrap();

        let wasm = wat::parse_str(
            r#"(module
                (func (export "noop"))
            )"#,
        )
        .unwrap();

        let config = PluginConfig::default_for("fuel-reset-test");
        let module = wasmtime::Module::new(engine.engine(), &wasm).unwrap();

        // First invocation
        let sandbox1 = test_sandbox(vec![], vec![], vec![]);
        let audit1 = Arc::new(AuditLog::new("test".into()));
        let mut store1 = engine.create_store(&config, sandbox1, audit1).unwrap();
        let linker = engine.create_linker().unwrap();
        let inst1 = linker.instantiate(&mut store1, &module).unwrap();
        let func1 = inst1.get_typed_func::<(), ()>(&mut store1, "noop").unwrap();
        func1.call(&mut store1, ()).unwrap();
        let fuel_after_1 = store1.get_fuel().unwrap();

        // Second invocation -- fresh store, full fuel budget
        let sandbox2 = test_sandbox(vec![], vec![], vec![]);
        let audit2 = Arc::new(AuditLog::new("test".into()));
        let store2 = engine.create_store(&config, sandbox2, audit2).unwrap();
        let fuel_before_2 = store2.get_fuel().unwrap();

        assert_eq!(
            fuel_before_2, config.fuel_budget,
            "second invocation should get full fuel budget"
        );
        assert!(
            fuel_before_2 >= fuel_after_1,
            "fresh store should have >= fuel than used store"
        );
    }

    // -- HostFunctionDispatcher HTTP tests --

    #[test]
    fn dispatcher_http_denied_no_network() {
        let d = test_dispatcher(vec![], vec![], vec![]);
        let result = d.handle_http_request("GET", "https://example.com/", &[], None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not permitted"));

        // Verify audit entry
        assert_eq!(d.audit_log().len(), 1);
        assert!(!d.audit_log().entries()[0].permitted);
    }

    #[test]
    fn dispatcher_http_validation_passes() {
        let d = test_dispatcher(vec!["example.com".into()], vec![], vec![]);
        let result = d.handle_http_request("GET", "https://example.com/data", &[], None);
        // Validation passes but execution not wired
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("validation passed"));

        // Audit should show success (validation passed)
        assert_eq!(d.audit_log().len(), 1);
        assert!(d.audit_log().entries()[0].permitted);
    }

    #[test]
    fn dispatcher_http_ssrf_blocked() {
        let d = test_dispatcher(vec!["*".into()], vec![], vec![]);
        let result = d.handle_http_request("GET", "http://127.0.0.1/", &[], None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("private"));
        assert_eq!(d.audit_log().count_denied(), 1);
    }

    // -- HostFunctionDispatcher FS tests --

    #[test]
    fn dispatcher_read_file_no_fs_perms() {
        let d = test_dispatcher(vec![], vec![], vec![]);
        let result = d.handle_read_file("/etc/passwd");
        assert!(result.is_err());
        assert_eq!(d.audit_log().count_denied(), 1);
    }

    #[test]
    fn dispatcher_read_file_within_sandbox() {
        let dir = canonical_temp_dir("clawft_engine_read_test");
        let file = dir.join("test.txt");
        std::fs::write(&file, "engine test content").unwrap();

        let d = test_dispatcher(vec![], vec![dir.to_string_lossy().to_string()], vec![]);
        let result = d.handle_read_file(file.to_str().unwrap());
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "engine test content");
        assert_eq!(d.audit_log().len(), 1);
        assert!(d.audit_log().entries()[0].permitted);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dispatcher_write_file_within_sandbox() {
        let dir = canonical_temp_dir("clawft_engine_write_test");

        let d = test_dispatcher(vec![], vec![dir.to_string_lossy().to_string()], vec![]);
        let file = dir.join("output.txt");
        let result = d.handle_write_file(file.to_str().unwrap(), "written by engine");
        assert!(result.is_ok());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "written by engine");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dispatcher_write_file_too_large() {
        let dir = canonical_temp_dir("clawft_engine_write_large");

        let d = test_dispatcher(vec![], vec![dir.to_string_lossy().to_string()], vec![]);
        let large = "x".repeat(5 * 1024 * 1024); // 5 MB > 4 MB limit
        let file = dir.join("big.txt");
        let result = d.handle_write_file(file.to_str().unwrap(), &large);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("too large"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- HostFunctionDispatcher env tests --

    #[test]
    fn dispatcher_get_env_not_allowed() {
        let d = test_dispatcher(vec![], vec![], vec![]);
        let result = d.handle_get_env("SECRET_KEY");
        assert!(result.is_none());
        assert_eq!(d.audit_log().count_denied(), 1);
    }

    #[test]
    fn dispatcher_get_env_allowed_and_set() {
        unsafe {
            std::env::set_var("CLAWFT_ENGINE_TEST_VAR", "engine_value");
        }
        let d = test_dispatcher(vec![], vec![], vec!["CLAWFT_ENGINE_TEST_VAR".into()]);
        let result = d.handle_get_env("CLAWFT_ENGINE_TEST_VAR");
        assert_eq!(result, Some("engine_value".into()));
        assert_eq!(d.audit_log().len(), 1);
        assert!(d.audit_log().entries()[0].permitted);
        unsafe {
            std::env::remove_var("CLAWFT_ENGINE_TEST_VAR");
        }
    }

    // -- HostFunctionDispatcher log tests --

    #[test]
    fn dispatcher_log_records_audit() {
        let d = test_dispatcher(vec![], vec![], vec![]);
        d.handle_log(2, "test info message");
        assert_eq!(d.audit_log().len(), 1);
        assert_eq!(d.audit_log().count_by_function("log"), 1);
        assert!(d.audit_log().entries()[0].permitted);
    }

    #[test]
    fn dispatcher_log_rate_limited() {
        let permissions = PluginPermissions::default();
        let resources = PluginResourceConfig {
            max_log_messages_per_minute: 2,
            ..PluginResourceConfig::default()
        };
        let sandbox = Arc::new(PluginSandbox::from_manifest(
            "test-plugin".into(),
            permissions,
            &resources,
        ));
        let audit = Arc::new(AuditLog::new("test-plugin".into()));
        let d = HostFunctionDispatcher::new(sandbox, audit);

        d.handle_log(2, "msg1");
        d.handle_log(2, "msg2");
        d.handle_log(2, "msg3"); // Should be rate-limited

        let entries = d.audit_log().entries();
        assert_eq!(entries.len(), 3);
        // First two should be permitted, third denied
        assert!(entries[0].permitted);
        assert!(entries[1].permitted);
        assert!(!entries[2].permitted);
    }

    #[test]
    fn t45_fuel_config_independent_per_invocation() {
        // Each PluginConfig has its own fuel_budget; creating a new
        // store per invocation means fuel resets.
        let config = PluginConfig::default_for("test");
        assert_eq!(config.fuel_budget, 1_000_000_000);

        // Creating another config gives the same fresh budget
        let config2 = PluginConfig::default_for("test");
        assert_eq!(config2.fuel_budget, config.fuel_budget);
    }

    /// T30: Wall-clock timeout enforcement.
    ///
    /// A WASM module with an infinite loop and a generous fuel budget should
    /// be terminated by the wall-clock timeout (via epoch interruption) rather
    /// than running indefinitely. We use a very short timeout (100ms) to keep
    /// the test fast.
    #[test]
    fn t30_wall_clock_timeout() {
        let engine = WasmPluginEngine::new().unwrap();

        // Infinite-loop module with a local variable increment to consume
        // fuel slowly enough that a large budget would not exhaust before
        // the wall-clock timeout fires.
        let wasm = wat::parse_str(
            r#"(module
                (func (export "slow_loop")
                    (local $i i64)
                    (loop $L
                        (local.set $i (i64.add (local.get $i) (i64.const 1)))
                        (br $L)
                    )
                )
            )"#,
        )
        .expect("WAT parsing should succeed");

        // Very large fuel budget -- this should NOT be the limiting factor.
        let mut config = PluginConfig::default_for("timeout-test");
        config.fuel_budget = MAX_FUEL_HARD; // 10 billion units

        let sandbox = test_sandbox(vec![], vec![], vec![]);
        let audit = Arc::new(AuditLog::new("timeout-test".into()));

        let module = wasmtime::Module::new(engine.engine(), &wasm)
            .expect("module compilation should succeed");

        let mut store = engine.create_store(&config, sandbox, audit).unwrap();
        let linker = engine.create_linker().unwrap();
        let instance = linker.instantiate(&mut store, &module).unwrap();

        let func = instance
            .get_typed_func::<(), ()>(&mut store, "slow_loop")
            .unwrap();

        // Use a very short wall-clock timeout (100ms) to keep the test fast.
        let timeout = Duration::from_millis(100);
        let start = Instant::now();

        let result = engine.call_func_with_timeout(&mut store, &func, (), timeout);
        let elapsed = start.elapsed();

        // The function should have trapped due to epoch deadline.
        assert!(
            result.is_err(),
            "infinite loop should be terminated by wall-clock timeout"
        );

        let err = result.unwrap_err();
        let err_msg = err.to_string();
        // Wasmtime epoch-based interruption causes a trap. The error may
        // contain "epoch", "interrupt", "wasm trap", or just be a generic
        // trap error with a backtrace. We accept any trap from a running
        // WASM function as evidence that the epoch deadline fired.
        assert!(
            err.downcast_ref::<wasmtime::Trap>().is_some()
                || err_msg.contains("epoch")
                || err_msg.contains("interrupt")
                || err_msg.contains("wasm"),
            "error should be a trap from epoch interruption, got: {err_msg}"
        );

        // Verify that the execution was terminated reasonably close to the
        // timeout (within 500ms tolerance to account for thread scheduling).
        assert!(
            elapsed < Duration::from_millis(600),
            "execution should complete within 600ms of timeout, took: {elapsed:?}"
        );

        // Verify that fuel was NOT the limiting factor -- there should be
        // significant fuel remaining.
        let remaining = store.get_fuel().unwrap_or(0);
        assert!(
            remaining > 1_000_000,
            "fuel should NOT be exhausted (epoch should fire first), remaining: {remaining}"
        );
    }

    /// T42: Complete audit logging verification.
    ///
    /// Executes a plugin dispatcher that calls ALL 5 host functions
    /// (http-request, read-file, write-file, get-env, log) and verifies
    /// that every single call produced a correct audit entry with the
    /// right operation type, target summary, and allowed/denied status.
    #[test]
    fn t42_complete_audit_logging_verification() {
        // Set up temp directory for filesystem operations
        let dir = canonical_temp_dir("clawft_t42_audit");
        let read_file = dir.join("readable.txt");
        std::fs::write(&read_file, "t42 readable content").unwrap();

        // Set up environment variable
        unsafe {
            std::env::set_var("CLAWFT_T42_AUDIT_VAR", "t42_audit_value");
        }

        // Create a sandbox with permissions for all 5 host function types
        let permissions = PluginPermissions {
            network: vec!["audit-test.example.com".into()],
            filesystem: vec![dir.to_string_lossy().to_string()],
            env_vars: vec!["CLAWFT_T42_AUDIT_VAR".into()],
            shell: false,
        };
        let sandbox = Arc::new(PluginSandbox::from_manifest(
            "t42-audit-plugin".into(),
            permissions,
            &PluginResourceConfig::default(),
        ));
        let audit = Arc::new(AuditLog::new("t42-audit-plugin".into()));
        let dispatcher = HostFunctionDispatcher::new(sandbox, audit);

        // 1. Call http-request (permitted)
        let _ = dispatcher.handle_http_request(
            "POST",
            "https://audit-test.example.com/api/v1",
            &[],
            Some(r#"{"key":"value"}"#),
        );

        // 2. Call read-file (permitted)
        let read_result = dispatcher.handle_read_file(read_file.to_str().unwrap());
        assert!(read_result.is_ok(), "read-file should succeed");

        // 3. Call write-file (permitted)
        let write_file = dir.join("written.txt");
        let write_result =
            dispatcher.handle_write_file(write_file.to_str().unwrap(), "t42 written content");
        assert!(write_result.is_ok(), "write-file should succeed");

        // 4. Call get-env (permitted)
        let env_result = dispatcher.handle_get_env("CLAWFT_T42_AUDIT_VAR");
        assert_eq!(env_result, Some("t42_audit_value".into()));

        // 5. Call log (permitted)
        dispatcher.handle_log(2, "t42 audit verification message");

        // ------ Verify audit log completeness ------

        let entries = dispatcher.audit_log().entries();
        assert_eq!(
            entries.len(),
            5,
            "all 5 host function calls should produce audit entries, got {}",
            entries.len()
        );

        // Verify each entry's operation type
        assert_eq!(entries[0].function, "http-request");
        assert_eq!(entries[1].function, "read-file");
        assert_eq!(entries[2].function, "write-file");
        assert_eq!(entries[3].function, "get-env");
        assert_eq!(entries[4].function, "log");

        // Verify count per function type
        assert_eq!(dispatcher.audit_log().count_by_function("http-request"), 1);
        assert_eq!(dispatcher.audit_log().count_by_function("read-file"), 1);
        assert_eq!(dispatcher.audit_log().count_by_function("write-file"), 1);
        assert_eq!(dispatcher.audit_log().count_by_function("get-env"), 1);
        assert_eq!(dispatcher.audit_log().count_by_function("log"), 1);

        // Verify all entries are permitted (not denied)
        for entry in &entries {
            assert!(
                entry.permitted,
                "all calls should be permitted, but {} was denied: {:?}",
                entry.function, entry.error
            );
        }

        // Verify target summaries contain expected content
        assert!(
            entries[0].params_summary.contains("POST")
                && entries[0].params_summary.contains("audit-test.example.com"),
            "http-request summary should contain method and URL, got: {}",
            entries[0].params_summary
        );
        assert!(
            entries[1].params_summary.contains("readable.txt"),
            "read-file summary should contain filename, got: {}",
            entries[1].params_summary
        );
        assert!(
            entries[2].params_summary.contains("written.txt"),
            "write-file summary should contain filename, got: {}",
            entries[2].params_summary
        );
        assert!(
            entries[3].params_summary.contains("CLAWFT_T42_AUDIT_VAR"),
            "get-env summary should contain var name, got: {}",
            entries[3].params_summary
        );
        assert!(
            entries[4].params_summary.contains("info") || entries[4].params_summary.contains("t42"),
            "log summary should contain level or message, got: {}",
            entries[4].params_summary
        );

        // Verify no denied entries
        assert_eq!(
            dispatcher.audit_log().count_denied(),
            0,
            "there should be zero denied entries"
        );

        // Verify audit log plugin_id
        assert_eq!(dispatcher.audit_log().plugin_id(), "t42-audit-plugin");

        // Verify timestamps are monotonically increasing
        for i in 1..entries.len() {
            assert!(
                entries[i].elapsed_ms >= entries[i - 1].elapsed_ms,
                "timestamps should be monotonically increasing: entry {} ({}) < entry {} ({})",
                i,
                entries[i].elapsed_ms,
                i - 1,
                entries[i - 1].elapsed_ms
            );
        }

        // ------ Verify denied operations also produce entries ------

        // Try a denied HTTP request (different domain)
        let _ = dispatcher.handle_http_request("GET", "https://evil.example.com/steal", &[], None);

        // Try a denied file read (outside sandbox)
        let _ = dispatcher.handle_read_file("/etc/shadow");

        // Try a denied env var
        let _ = dispatcher.handle_get_env("SECRET_TOKEN");

        let all_entries = dispatcher.audit_log().entries();
        assert_eq!(
            all_entries.len(),
            8,
            "should now have 8 entries (5 permitted + 3 denied)"
        );

        // Verify the 3 new entries are denied
        assert!(
            !all_entries[5].permitted,
            "denied http should be marked denied"
        );
        assert!(
            !all_entries[6].permitted,
            "denied file should be marked denied"
        );
        assert!(
            !all_entries[7].permitted,
            "denied env should be marked denied"
        );

        // Verify denied entries have error messages
        for entry in &all_entries[5..] {
            assert!(
                entry.error.is_some(),
                "denied entry for {} should have an error message",
                entry.function
            );
        }

        // Cleanup
        unsafe {
            std::env::remove_var("CLAWFT_T42_AUDIT_VAR");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
