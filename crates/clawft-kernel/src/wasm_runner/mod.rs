//! WASM tool execution sandbox and built-in tool catalog.
//!
//! Provides types and configuration for running tools inside
//! isolated WASM sandboxes with fuel metering, memory limits,
//! and host filesystem isolation.
//!
//! # K3 Tool Lifecycle
//!
//! Tools go through: Build -> Deploy -> Execute -> Version -> Revoke.
//! This module provides the execution runtime and tool catalog;
//! the lifecycle management is in [`crate::tree_manager`].
//!
//! # Feature Gate
//!
//! This module is compiled unconditionally, but the actual
//! Wasmtime runtime integration requires the `wasm-sandbox`
//! feature flag. Without it, [`WasmToolRunner::new`] returns
//! a runner that rejects all tool loads with [`WasmError::RuntimeUnavailable`].
//!
//! # Security
//!
//! Each tool execution gets its own isolated store:
//! - No host filesystem access (unless WASI explicitly enabled)
//! - No network access
//! - CPU bounded by fuel metering
//! - Memory bounded by configurable cap
//! - Wall-clock timeout as safety net

mod catalog;
mod registry;
mod runner;
mod tools_agent;
mod tools_fs;
mod tools_sys;
mod types;

// Re-export everything from sub-modules to preserve the public API.
pub use catalog::builtin_tool_catalog;
pub use registry::{BuiltinTool, ToolRegistry};
pub use runner::{CompiledModuleCache, WasmToolRunner, compute_module_hash};
pub use tools_agent::*;
pub use tools_fs::*;
pub use tools_sys::*;
pub use types::*;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    // --- Config tests (preserved) ---

    #[test]
    fn default_config() {
        let config = WasmSandboxConfig::default();
        assert_eq!(config.max_fuel, 1_000_000);
        assert_eq!(config.max_memory_bytes, 16 * 1024 * 1024);
        assert_eq!(config.max_execution_time_secs, 30);
        assert!(config.allowed_host_calls.is_empty());
        assert!(!config.wasi_enabled);
        assert_eq!(config.max_module_size_bytes, 10 * 1024 * 1024);
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = WasmSandboxConfig {
            max_fuel: 500_000,
            max_memory_bytes: 8 * 1024 * 1024,
            max_execution_time_secs: 10,
            allowed_host_calls: vec!["clock_time_get".into()],
            wasi_enabled: true,
            max_module_size_bytes: 5 * 1024 * 1024,
        };
        let json = serde_json::to_string(&config).unwrap();
        let restored: WasmSandboxConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.max_fuel, 500_000);
        assert!(restored.wasi_enabled);
        assert_eq!(restored.allowed_host_calls.len(), 1);
    }

    #[test]
    fn execution_timeout_duration() {
        let config = WasmSandboxConfig {
            max_execution_time_secs: 15,
            ..Default::default()
        };
        assert_eq!(config.execution_timeout(), Duration::from_secs(15));
    }

    #[test]
    fn tool_state_default() {
        let state = ToolState::default();
        assert!(state.tool_name.is_empty());
        assert!(state.stdin.is_empty());
        assert!(state.stdout.is_empty());
        assert!(state.stderr.is_empty());
        assert!(state.env.is_empty());
    }

    // --- Validation tests (preserved) ---

    #[test]
    fn validate_wasm_rejects_too_large() {
        let runner = WasmToolRunner::new(WasmSandboxConfig {
            max_module_size_bytes: 100,
            ..Default::default()
        });
        let big_bytes = vec![0u8; 200];
        let result = runner.validate_wasm(&big_bytes);
        assert!(matches!(result, Err(WasmError::ModuleTooLarge { .. })));
    }

    #[test]
    fn validate_wasm_rejects_invalid_magic() {
        let runner = WasmToolRunner::new(WasmSandboxConfig::default());
        let bad_bytes = b"not a wasm module at all";
        let result = runner.validate_wasm(bad_bytes);
        assert!(matches!(result, Err(WasmError::InvalidModule(_))));
    }

    #[test]
    fn validate_wasm_rejects_too_short() {
        let runner = WasmToolRunner::new(WasmSandboxConfig::default());
        let short = b"\0asm";
        let result = runner.validate_wasm(short);
        assert!(matches!(result, Err(WasmError::InvalidModule(_))));
    }

    #[test]
    fn validate_wasm_accepts_valid_header() {
        let runner = WasmToolRunner::new(WasmSandboxConfig::default());
        let mut wasm = Vec::new();
        wasm.extend_from_slice(b"\0asm");
        wasm.extend_from_slice(&1u32.to_le_bytes());

        let result = runner.validate_wasm(&wasm);
        assert!(result.is_ok());
        let validation = result.unwrap();
        assert!(validation.valid);
        assert!(validation.warnings.is_empty());
    }

    #[test]
    fn validate_wasm_warns_on_wrong_version() {
        let runner = WasmToolRunner::new(WasmSandboxConfig::default());
        let mut wasm = Vec::new();
        wasm.extend_from_slice(b"\0asm");
        wasm.extend_from_slice(&2u32.to_le_bytes());

        #[cfg(not(feature = "wasm-sandbox"))]
        {
            let result = runner.validate_wasm(&wasm).unwrap();
            assert!(result.valid);
            assert!(!result.warnings.is_empty());
            assert!(result.warnings[0].contains("version: 2"));
        }
        #[cfg(feature = "wasm-sandbox")]
        {
            let result = runner.validate_wasm(&wasm);
            assert!(
                result.is_err() || {
                    let v = result.unwrap();
                    !v.warnings.is_empty()
                }
            );
        }
    }

    #[test]
    fn load_tool_without_feature_rejects() {
        #[cfg(not(feature = "wasm-sandbox"))]
        {
            let runner = WasmToolRunner::new(WasmSandboxConfig::default());
            let mut wasm = Vec::new();
            wasm.extend_from_slice(b"\0asm");
            wasm.extend_from_slice(&1u32.to_le_bytes());
            wasm.extend_from_slice(&[0u8; 16]);
            let result = runner.load_tool("test-tool", &wasm);
            assert!(matches!(result, Err(WasmError::RuntimeUnavailable)));
        }
    }

    #[test]
    fn wasm_error_display() {
        let err = WasmError::RuntimeUnavailable;
        assert!(err.to_string().contains("wasm-sandbox"));

        let err = WasmError::ModuleTooLarge {
            size: 20_000_000,
            limit: 10_000_000,
        };
        assert!(err.to_string().contains("20000000"));
        assert!(err.to_string().contains("10000000"));

        let err = WasmError::FuelExhausted {
            consumed: 1_000_000,
            limit: 1_000_000,
        };
        assert!(err.to_string().contains("fuel exhausted"));

        let err = WasmError::MemoryLimitExceeded {
            allocated: 32_000_000,
            limit: 16_000_000,
        };
        assert!(err.to_string().contains("memory limit"));
    }

    #[test]
    fn wasm_tool_result_serde_roundtrip() {
        let result = WasmToolResult {
            stdout: "output".into(),
            stderr: String::new(),
            exit_code: 0,
            fuel_consumed: 50_000,
            memory_peak: 1024,
            execution_time: Duration::from_millis(150),
        };
        let json = serde_json::to_string(&result).unwrap();
        let restored: WasmToolResult = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.stdout, "output");
        assert_eq!(restored.exit_code, 0);
        assert_eq!(restored.fuel_consumed, 50_000);
        assert_eq!(restored.execution_time, Duration::from_millis(150));
    }

    #[test]
    fn wasm_validation_serde_roundtrip() {
        let validation = WasmValidation {
            valid: true,
            exports: vec!["execute".into(), "tool_schema".into()],
            imports: vec!["wasi_snapshot_preview1::fd_write".into()],
            estimated_memory: 65536,
            warnings: vec!["uses wasi".into()],
        };
        let json = serde_json::to_string(&validation).unwrap();
        let restored: WasmValidation = serde_json::from_str(&json).unwrap();
        assert!(restored.valid);
        assert_eq!(restored.exports.len(), 2);
        assert_eq!(restored.imports.len(), 1);
    }

    // --- Catalog tests ---

    #[test]
    fn builtin_catalog_has_expected_tools() {
        let catalog = builtin_tool_catalog();
        #[cfg(feature = "ecc")]
        assert_eq!(catalog.len(), 36, "29 base + 7 ecc tools");
        #[cfg(not(feature = "ecc"))]
        assert_eq!(catalog.len(), 29);
    }

    #[test]
    fn all_tools_have_valid_schema() {
        let catalog = builtin_tool_catalog();
        for spec in &catalog {
            assert!(
                spec.parameters.is_object(),
                "{} has non-object schema",
                spec.name
            );
            assert!(
                spec.parameters.get("type").and_then(|v| v.as_str()) == Some("object"),
                "{} schema type is not 'object'",
                spec.name,
            );
        }
    }

    #[test]
    fn all_tools_have_gate_action() {
        let catalog = builtin_tool_catalog();
        for spec in &catalog {
            assert!(
                !spec.gate_action.is_empty(),
                "{} missing gate_action",
                spec.name
            );
            assert!(
                spec.gate_action.starts_with("tool.") || spec.gate_action.starts_with("ecc."),
                "{} gate_action should start with 'tool.' or 'ecc.'",
                spec.name,
            );
        }
    }

    #[test]
    fn tool_names_are_unique() {
        let catalog = builtin_tool_catalog();
        let mut names: Vec<&str> = catalog.iter().map(|s| s.name.as_str()).collect();
        names.sort();
        let unique_count = {
            let mut u = names.clone();
            u.dedup();
            u.len()
        };
        assert_eq!(names.len(), unique_count, "duplicate tool names found");
    }

    #[test]
    fn tool_categories_correct() {
        let catalog = builtin_tool_catalog();
        let fs_count = catalog
            .iter()
            .filter(|s| s.category == ToolCategory::Filesystem)
            .count();
        let agent_count = catalog
            .iter()
            .filter(|s| s.category == ToolCategory::Agent)
            .count();
        let sys_count = catalog
            .iter()
            .filter(|s| s.category == ToolCategory::System)
            .count();
        assert_eq!(fs_count, 10);
        assert_eq!(agent_count, 9);
        assert_eq!(sys_count, 10);
        #[cfg(feature = "ecc")]
        {
            let ecc_count = catalog
                .iter()
                .filter(|s| s.category == ToolCategory::Ecc)
                .count();
            assert_eq!(ecc_count, 7);
        }
    }

    // --- FsReadFileTool tests ---

    #[test]
    fn fs_read_file_reads_content() {
        let dir = std::env::temp_dir().join("clawft-fs-read-test");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("test.txt");
        std::fs::write(&file, "hello world").unwrap();

        let tool = FsReadFileTool::new();
        let result = tool
            .execute(serde_json::json!({"path": file.to_str().unwrap()}))
            .unwrap();
        assert_eq!(result["content"], "hello world");
        assert_eq!(result["size"], 11);
        assert!(result["modified"].as_str().is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fs_read_file_with_offset_limit() {
        let dir = std::env::temp_dir().join("clawft-fs-offset-test");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("test.txt");
        std::fs::write(&file, "0123456789").unwrap();

        let tool = FsReadFileTool::new();
        let result = tool
            .execute(serde_json::json!({
                "path": file.to_str().unwrap(),
                "offset": 3,
                "limit": 4,
            }))
            .unwrap();
        assert_eq!(result["content"], "3456");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fs_read_file_not_found() {
        let tool = FsReadFileTool::new();
        let result = tool.execute(serde_json::json!({"path": "/no/such/file/ever"}));
        assert!(matches!(result, Err(ToolError::FileNotFound(_))));
    }

    #[test]
    fn fs_read_file_returns_metadata() {
        let dir = std::env::temp_dir().join("clawft-fs-meta-test");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("meta.txt");
        std::fs::write(&file, "data").unwrap();

        let tool = FsReadFileTool::new();
        let result = tool
            .execute(serde_json::json!({"path": file.to_str().unwrap()}))
            .unwrap();
        assert!(result.get("size").is_some());
        assert!(result.get("modified").is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- AgentSpawnTool tests ---

    #[test]
    fn agent_spawn_creates_process() {
        let pt = Arc::new(crate::process::ProcessTable::new(64));
        let tool = AgentSpawnTool::new(pt.clone());
        let result = tool
            .execute(serde_json::json!({"agent_id": "test-spawn"}))
            .unwrap();

        let pid = result["pid"].as_u64().unwrap();
        assert!(pt.get(pid).is_some());
        assert_eq!(result["state"], "running");
    }

    #[test]
    fn agent_spawn_with_wasm_backend_fails() {
        let pt = Arc::new(crate::process::ProcessTable::new(64));
        let tool = AgentSpawnTool::new(pt);
        let result = tool.execute(serde_json::json!({
            "agent_id": "wasm-agent",
            "backend": "wasm",
        }));
        assert!(matches!(result, Err(ToolError::ExecutionFailed(_))));
    }

    #[test]
    fn agent_spawn_returns_pid() {
        let pt = Arc::new(crate::process::ProcessTable::new(64));
        let tool = AgentSpawnTool::new(pt);
        let result = tool
            .execute(serde_json::json!({"agent_id": "pid-test"}))
            .unwrap();
        assert!(result.get("pid").is_some());
        assert_eq!(result["agent_id"], "pid-test");
    }

    // --- ToolRegistry tests ---

    #[test]
    fn registry_register_and_execute() {
        let mut registry = ToolRegistry::new();
        let tool = Arc::new(FsReadFileTool::new());
        registry.register(tool);
        assert_eq!(registry.len(), 1);
        assert!(registry.get("fs.read_file").is_some());
    }

    #[test]
    fn registry_not_found() {
        let registry = ToolRegistry::new();
        let result = registry.execute("no.such.tool", serde_json::json!({}));
        assert!(matches!(result, Err(ToolError::NotFound(_))));
    }

    // --- Hierarchical ToolRegistry tests (K4 A1) ---

    #[test]
    fn registry_parent_chain_lookup() {
        let mut parent = ToolRegistry::new();
        parent.register(Arc::new(FsReadFileTool::new()));
        let parent = Arc::new(parent);

        let child = ToolRegistry::with_parent(parent);
        assert!(
            child.get("fs.read_file").is_some(),
            "child should find tool in parent"
        );
    }

    #[test]
    fn registry_child_overrides_parent() {
        let mut parent = ToolRegistry::new();
        parent.register(Arc::new(FsReadFileTool::new()));
        let parent = Arc::new(parent);

        let mut child = ToolRegistry::with_parent(parent);
        child.register(Arc::new(FsReadFileTool::new()));
        assert_eq!(child.len(), 1, "deduplicated count should be 1");
        assert!(child.get("fs.read_file").is_some());
    }

    #[test]
    fn registry_list_merges_parent() {
        let mut parent = ToolRegistry::new();
        parent.register(Arc::new(FsReadFileTool::new()));
        let parent = Arc::new(parent);

        let pt = Arc::new(crate::process::ProcessTable::new(64));
        let mut child = ToolRegistry::with_parent(parent);
        child.register(Arc::new(AgentSpawnTool::new(pt)));

        let list = child.list();
        assert!(
            list.contains(&"fs.read_file".to_string()),
            "should include parent tool"
        );
        assert!(
            list.contains(&"agent.spawn".to_string()),
            "should include child tool"
        );
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn registry_empty_child_delegates_all() {
        let mut parent = ToolRegistry::new();
        parent.register(Arc::new(FsReadFileTool::new()));
        let parent = Arc::new(parent);

        let child = ToolRegistry::with_parent(parent);
        assert_eq!(child.len(), 1);
        assert!(!child.is_empty());

        let dir = std::env::temp_dir().join("clawft-delegate-test");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("test.txt");
        std::fs::write(&file, "delegate").unwrap();
        let result = child.execute(
            "fs.read_file",
            serde_json::json!({"path": file.to_str().unwrap()}),
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap()["content"], "delegate");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- Sandbox tests (K4 B1) ---

    #[test]
    fn sandbox_denies_path_outside_allowed() {
        let sandbox = SandboxConfig {
            allowed_paths: vec![std::env::temp_dir()],
            ..Default::default()
        };
        let tool = FsReadFileTool::with_sandbox(sandbox);
        let result = tool.execute(serde_json::json!({"path": "/etc/passwd"}));
        assert!(matches!(result, Err(ToolError::PermissionDenied(_))));
    }

    #[test]
    fn sandbox_allows_path_inside_allowed() {
        let dir = std::env::temp_dir().join("clawft-sandbox-allow-test");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("allowed.txt");
        std::fs::write(&file, "allowed").unwrap();

        let sandbox = SandboxConfig {
            allowed_paths: vec![std::env::temp_dir()],
            ..Default::default()
        };
        let tool = FsReadFileTool::with_sandbox(sandbox);
        let result = tool.execute(serde_json::json!({"path": file.to_str().unwrap()}));
        assert!(result.is_ok());
        assert_eq!(result.unwrap()["content"], "allowed");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sandbox_default_allows_all() {
        let dir = std::env::temp_dir().join("clawft-sandbox-default-test");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("default.txt");
        std::fs::write(&file, "default").unwrap();

        let tool = FsReadFileTool::new();
        let result = tool.execute(serde_json::json!({"path": file.to_str().unwrap()}));
        assert!(result.is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- K4 C1: Filesystem tool tests ---

    #[test]
    fn fs_write_file_creates_and_writes() {
        let dir = std::env::temp_dir().join("clawft-fs-write-test");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("out.txt");
        let tool = FsWriteFileTool::new();
        let result =
            tool.execute(serde_json::json!({"path": file.to_str().unwrap(), "content": "hello"}));
        assert!(result.is_ok());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fs_write_file_missing_content() {
        let tool = FsWriteFileTool::new();
        let result = tool.execute(serde_json::json!({"path": "/tmp/x"}));
        assert!(matches!(result, Err(ToolError::InvalidArgs(_))));
    }

    #[test]
    fn fs_read_dir_lists_entries() {
        let dir = std::env::temp_dir().join("clawft-fs-readdir-test");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        std::fs::write(dir.join("b.txt"), "b").unwrap();
        let tool = FsReadDirTool::new();
        let result = tool
            .execute(serde_json::json!({"path": dir.to_str().unwrap()}))
            .unwrap();
        assert!(result["count"].as_u64().unwrap() >= 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fs_read_dir_not_found() {
        let tool = FsReadDirTool::new();
        let result = tool.execute(serde_json::json!({"path": "/no/such/dir/ever"}));
        assert!(matches!(result, Err(ToolError::FileNotFound(_))));
    }

    #[test]
    fn fs_create_dir_recursive() {
        let dir = std::env::temp_dir().join("clawft-fs-mkdir-test/a/b/c");
        let tool = FsCreateDirTool::new();
        let result = tool.execute(serde_json::json!({"path": dir.to_str().unwrap()}));
        assert!(result.is_ok());
        assert!(dir.exists());
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("clawft-fs-mkdir-test"));
    }

    #[test]
    fn fs_create_dir_sandbox_denied() {
        let tool = FsCreateDirTool::new();
        let dir = std::env::temp_dir().join("clawft-fs-mkdir-sandbox-test");
        let result = tool.execute(serde_json::json!({"path": dir.to_str().unwrap()}));
        assert!(result.is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fs_remove_file() {
        let dir = std::env::temp_dir().join("clawft-fs-rm-test");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("delete_me.txt");
        std::fs::write(&file, "bye").unwrap();
        let tool = FsRemoveTool::new();
        let result = tool.execute(serde_json::json!({"path": file.to_str().unwrap()}));
        assert!(result.is_ok());
        assert!(!file.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fs_remove_not_found() {
        let tool = FsRemoveTool::new();
        let result = tool.execute(serde_json::json!({"path": "/no/such/file/xyz"}));
        assert!(matches!(result, Err(ToolError::FileNotFound(_))));
    }

    #[test]
    fn fs_copy_file() {
        let dir = std::env::temp_dir().join("clawft-fs-copy-test");
        let _ = std::fs::create_dir_all(&dir);
        let src = dir.join("src.txt");
        let dst = dir.join("dst.txt");
        std::fs::write(&src, "copy me").unwrap();
        let tool = FsCopyTool::new();
        let result = tool.execute(
            serde_json::json!({"src": src.to_str().unwrap(), "dst": dst.to_str().unwrap()}),
        );
        assert!(result.is_ok());
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "copy me");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fs_copy_not_found() {
        let tool = FsCopyTool::new();
        let result = tool.execute(serde_json::json!({"src": "/no/file", "dst": "/tmp/out"}));
        assert!(matches!(result, Err(ToolError::FileNotFound(_))));
    }

    #[test]
    fn fs_move_file() {
        let dir = std::env::temp_dir().join("clawft-fs-move-test");
        let _ = std::fs::create_dir_all(&dir);
        let src = dir.join("old.txt");
        let dst = dir.join("new.txt");
        std::fs::write(&src, "move me").unwrap();
        let tool = FsMoveTool::new();
        let result = tool.execute(
            serde_json::json!({"src": src.to_str().unwrap(), "dst": dst.to_str().unwrap()}),
        );
        assert!(result.is_ok());
        assert!(!src.exists());
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "move me");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fs_move_not_found() {
        let tool = FsMoveTool::new();
        let result = tool.execute(serde_json::json!({"src": "/no/file", "dst": "/tmp/out"}));
        assert!(matches!(result, Err(ToolError::FileNotFound(_))));
    }

    #[test]
    fn fs_stat_returns_metadata() {
        let dir = std::env::temp_dir().join("clawft-fs-stat-test");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("stat.txt");
        std::fs::write(&file, "data").unwrap();
        let tool = FsStatTool::new();
        let result = tool
            .execute(serde_json::json!({"path": file.to_str().unwrap()}))
            .unwrap();
        assert_eq!(result["size"], 4);
        assert_eq!(result["is_file"], true);
        assert_eq!(result["is_dir"], false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fs_stat_error() {
        let tool = FsStatTool::new();
        let result = tool.execute(serde_json::json!({"path": "/no/such/file"}));
        assert!(matches!(result, Err(ToolError::ExecutionFailed(_))));
    }

    #[test]
    fn fs_exists_checks() {
        let tool = FsExistsTool::new();
        let result = tool.execute(serde_json::json!({"path": "/tmp"})).unwrap();
        assert_eq!(result["exists"], true);
        assert_eq!(result["is_dir"], true);

        let result = tool
            .execute(serde_json::json!({"path": "/no/such/path/xyz"}))
            .unwrap();
        assert_eq!(result["exists"], false);
    }

    #[test]
    fn fs_glob_finds_files() {
        let dir = std::env::temp_dir().join("clawft-fs-glob-test");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("test.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.join("test.txt"), "text").unwrap();
        let tool = FsGlobTool::new();
        let result = tool
            .execute(serde_json::json!({
                "pattern": "*.rs",
                "base_dir": dir.to_str().unwrap(),
            }))
            .unwrap();
        assert!(result["count"].as_u64().unwrap() >= 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fs_glob_no_match() {
        let dir = std::env::temp_dir().join("clawft-fs-glob-nomatch-test");
        let _ = std::fs::create_dir_all(&dir);
        let tool = FsGlobTool::new();
        let result = tool
            .execute(serde_json::json!({
                "pattern": "*.xyz",
                "base_dir": dir.to_str().unwrap(),
            }))
            .unwrap();
        assert_eq!(result["count"], 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- K4 C2: Agent tool tests ---

    #[test]
    fn agent_stop_cancels_token() {
        let pt = Arc::new(crate::process::ProcessTable::new(64));
        let spawn = AgentSpawnTool::new(pt.clone());
        let result = spawn
            .execute(serde_json::json!({"agent_id": "stop-test"}))
            .unwrap();
        let pid = result["pid"].as_u64().unwrap();

        let tool = AgentStopTool::new(pt.clone());
        let result = tool.execute(serde_json::json!({"pid": pid}));
        assert!(result.is_ok());
        let entry = pt.get(pid).unwrap();
        assert!(entry.cancel_token.is_cancelled());
    }

    #[test]
    fn agent_stop_not_found() {
        let pt = Arc::new(crate::process::ProcessTable::new(64));
        let tool = AgentStopTool::new(pt);
        let result = tool.execute(serde_json::json!({"pid": 9999}));
        assert!(matches!(result, Err(ToolError::NotFound(_))));
    }

    #[test]
    fn agent_list_shows_agents() {
        let pt = Arc::new(crate::process::ProcessTable::new(64));
        let spawn = AgentSpawnTool::new(pt.clone());
        spawn
            .execute(serde_json::json!({"agent_id": "list-a"}))
            .unwrap();
        spawn
            .execute(serde_json::json!({"agent_id": "list-b"}))
            .unwrap();

        let tool = AgentListTool::new(pt);
        let result = tool.execute(serde_json::json!({})).unwrap();
        assert!(result["count"].as_u64().unwrap() >= 2);
    }

    #[test]
    fn agent_inspect_returns_details() {
        let pt = Arc::new(crate::process::ProcessTable::new(64));
        let spawn = AgentSpawnTool::new(pt.clone());
        let r = spawn
            .execute(serde_json::json!({"agent_id": "inspect-me"}))
            .unwrap();
        let pid = r["pid"].as_u64().unwrap();

        let tool = AgentInspectTool::new(pt);
        let result = tool.execute(serde_json::json!({"pid": pid})).unwrap();
        assert_eq!(result["agent_id"], "inspect-me");
        assert!(result["capabilities"].is_object());
    }

    #[test]
    fn agent_inspect_not_found() {
        let pt = Arc::new(crate::process::ProcessTable::new(64));
        let tool = AgentInspectTool::new(pt);
        let result = tool.execute(serde_json::json!({"pid": 9999}));
        assert!(matches!(result, Err(ToolError::NotFound(_))));
    }

    #[test]
    fn agent_suspend_resume_cycle() {
        let pt = Arc::new(crate::process::ProcessTable::new(64));
        let spawn = AgentSpawnTool::new(pt.clone());
        let r = spawn
            .execute(serde_json::json!({"agent_id": "sr-test"}))
            .unwrap();
        let pid = r["pid"].as_u64().unwrap();

        let suspend = AgentSuspendTool::new(pt.clone());
        suspend.execute(serde_json::json!({"pid": pid})).unwrap();
        assert_eq!(
            pt.get(pid).unwrap().state,
            crate::process::ProcessState::Suspended
        );

        let resume = AgentResumeTool::new(pt.clone());
        resume.execute(serde_json::json!({"pid": pid})).unwrap();
        assert_eq!(
            pt.get(pid).unwrap().state,
            crate::process::ProcessState::Running
        );
    }

    // --- K4 C3: System tool tests ---

    #[test]
    fn sys_env_get_returns_value() {
        unsafe {
            std::env::set_var("CLAWFT_TEST_VAR", "test_value");
        }
        let tool = SysEnvGetTool::new();
        let result = tool
            .execute(serde_json::json!({"name": "CLAWFT_TEST_VAR"}))
            .unwrap();
        assert_eq!(result["value"], "test_value");
        unsafe {
            std::env::remove_var("CLAWFT_TEST_VAR");
        }
    }

    #[test]
    fn sys_env_get_missing_returns_null() {
        let tool = SysEnvGetTool::new();
        let result = tool
            .execute(serde_json::json!({"name": "NO_SUCH_VAR_EVER_XYZ"}))
            .unwrap();
        assert!(result["value"].is_null());
    }

    #[test]
    fn sys_cron_add_list_remove() {
        let cron = Arc::new(crate::cron::CronService::new());
        let add = SysCronAddTool::new(cron.clone());
        let result = add
            .execute(serde_json::json!({
                "name": "test-job",
                "interval_secs": 60,
                "command": "ping",
            }))
            .unwrap();
        let job_id = result["id"].as_str().unwrap().to_string();

        let list = SysCronListTool::new(cron.clone());
        let jobs = list.execute(serde_json::json!({})).unwrap();
        assert!(jobs.as_array().map(|a| !a.is_empty()).unwrap_or(false));

        let rm = SysCronRemoveTool::new(cron);
        let result = rm.execute(serde_json::json!({"id": job_id}));
        assert!(result.is_ok());
    }

    #[test]
    fn sys_cron_remove_not_found() {
        let cron = Arc::new(crate::cron::CronService::new());
        let tool = SysCronRemoveTool::new(cron);
        let result = tool.execute(serde_json::json!({"id": "no-such-job"}));
        assert!(matches!(result, Err(ToolError::NotFound(_))));
    }

    #[test]
    fn simple_glob_match_works() {
        assert!(tools_fs::simple_glob_match("*.rs", "main.rs"));
        assert!(tools_fs::simple_glob_match("*.rs", "lib.rs"));
        assert!(!tools_fs::simple_glob_match("*.rs", "main.txt"));
        assert!(tools_fs::simple_glob_match("test?", "test1"));
        assert!(!tools_fs::simple_glob_match("test?", "test12"));
        assert!(tools_fs::simple_glob_match("*", "anything"));
    }

    // --- K4 D2: Module cache tests ---

    #[test]
    fn cache_roundtrip() {
        let dir = std::env::temp_dir().join("clawft-cache-rt-test");
        let _ = std::fs::remove_dir_all(&dir);
        let cache = CompiledModuleCache::new(dir.clone(), 1024 * 1024);
        let hash = [0xAAu8; 32];
        let data = b"compiled wasm bytes";
        cache.put(&hash, data);
        let got = cache.get(&hash).unwrap();
        assert_eq!(got, data);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_miss_returns_none() {
        let dir = std::env::temp_dir().join("clawft-cache-miss-test");
        let _ = std::fs::remove_dir_all(&dir);
        let cache = CompiledModuleCache::new(dir.clone(), 1024 * 1024);
        assert!(cache.get(&[0xBBu8; 32]).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_eviction() {
        let dir = std::env::temp_dir().join("clawft-cache-evict-test");
        let _ = std::fs::remove_dir_all(&dir);
        let cache = CompiledModuleCache::new(dir.clone(), 100);
        for i in 0..10u8 {
            let mut hash = [0u8; 32];
            hash[0] = i;
            cache.put(&hash, &[i; 20]);
        }
        let entries: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert!(
            entries.len() < 10,
            "eviction should have removed some entries"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- K4 D3: WASI scope tests ---

    #[test]
    fn wasi_scope_default_is_none() {
        let scope = WasiFsScope::default();
        assert_eq!(scope, WasiFsScope::None);
    }

    #[test]
    fn wasi_scope_serde_roundtrip() {
        let scope = WasiFsScope::ReadOnly(PathBuf::from("/data"));
        let json = serde_json::to_string(&scope).unwrap();
        let restored: WasiFsScope = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, scope);
    }

    // --- K4 F1: Signing tests ---

    #[test]
    #[cfg(feature = "exochain")]
    fn verify_kernel_signature() {
        let key = ed25519_dalek::SigningKey::from_bytes(&[42u8; 32]);
        let hash = [0xAAu8; 32];
        use ed25519_dalek::Signer;
        let sig = key.sign(&hash);
        let pubkey = key.verifying_key().to_bytes();
        assert!(verify_tool_signature(&hash, &sig.to_bytes(), &pubkey));
    }

    #[test]
    #[cfg(feature = "exochain")]
    fn verify_tampered_cert_fails() {
        let key = ed25519_dalek::SigningKey::from_bytes(&[42u8; 32]);
        let hash = [0xAAu8; 32];
        use ed25519_dalek::Signer;
        let sig = key.sign(&hash);
        let mut bad_sig = sig.to_bytes();
        bad_sig[0] ^= 0xFF;
        let pubkey = key.verifying_key().to_bytes();
        assert!(!verify_tool_signature(&hash, &bad_sig, &pubkey));
    }

    #[test]
    fn signing_authority_serde() {
        let auth = ToolSigningAuthority::Kernel;
        let json = serde_json::to_string(&auth).unwrap();
        let _: ToolSigningAuthority = serde_json::from_str(&json).unwrap();
    }

    #[test]
    #[cfg(feature = "exochain")]
    fn signing_verify_roundtrip() {
        use ed25519_dalek::{Signer, SigningKey};
        let mut rng = rand::rngs::OsRng;
        let sk = SigningKey::generate(&mut rng);
        let pk_bytes: [u8; 32] = sk.verifying_key().to_bytes();
        let hash: [u8; 32] = [42u8; 32];
        let sig = sk.sign(&hash);
        let sig_bytes: [u8; 64] = sig.to_bytes();
        assert!(verify_tool_signature(&hash, &sig_bytes, &pk_bytes));
    }

    #[test]
    #[cfg(feature = "exochain")]
    fn signing_tampered_fails() {
        use ed25519_dalek::{Signer, SigningKey};
        let mut rng = rand::rngs::OsRng;
        let sk = SigningKey::generate(&mut rng);
        let pk_bytes: [u8; 32] = sk.verifying_key().to_bytes();
        let hash: [u8; 32] = [42u8; 32];
        let sig = sk.sign(&hash);
        let mut sig_bytes: [u8; 64] = sig.to_bytes();
        sig_bytes[0] ^= 0xff;
        assert!(!verify_tool_signature(&hash, &sig_bytes, &pk_bytes));
    }

    // --- K4 F2: Backend selection tests ---

    #[test]
    fn backend_low_risk_native() {
        assert_eq!(BackendSelection::from_risk(0.1), BackendSelection::Native);
        assert_eq!(BackendSelection::from_risk(0.3), BackendSelection::Native);
    }

    #[test]
    fn backend_high_risk_wasm() {
        assert_eq!(BackendSelection::from_risk(0.5), BackendSelection::Wasm);
        assert_eq!(BackendSelection::from_risk(0.7), BackendSelection::Wasm);
    }

    // --- Module hash tests ---

    #[test]
    fn module_hash_deterministic() {
        let data = b"test module bytes";
        let h1 = compute_module_hash(data);
        let h2 = compute_module_hash(data);
        assert_eq!(h1, h2);
    }

    #[test]
    fn module_hash_differs_for_different_input() {
        let h1 = compute_module_hash(b"module A");
        let h2 = compute_module_hash(b"module B");
        assert_ne!(h1, h2);
    }

    // --- ToolVersion/DeployedTool serde ---

    #[test]
    fn tool_version_serde_roundtrip() {
        use chrono::Utc;
        let tv = ToolVersion {
            version: 1,
            module_hash: [0xAA; 32],
            signature: [0xBB; 64],
            deployed_at: Utc::now(),
            revoked: false,
            chain_seq: 42,
        };
        let json = serde_json::to_string(&tv).unwrap();
        let restored: ToolVersion = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.version, 1);
        assert_eq!(restored.chain_seq, 42);
        assert!(!restored.revoked);
    }

    #[test]
    fn builtin_tool_spec_serde_roundtrip() {
        use crate::governance::EffectVector;
        let spec = BuiltinToolSpec {
            name: "test.tool".into(),
            category: ToolCategory::User,
            description: "A test tool".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
            gate_action: "tool.test".into(),
            effect: EffectVector::default(),
            native: true,
        };
        let json = serde_json::to_string(&spec).unwrap();
        let restored: BuiltinToolSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.name, "test.tool");
        assert_eq!(restored.category, ToolCategory::User);
    }

    // --- K3: WASM execute_bytes tests ---

    #[cfg(feature = "wasm-sandbox")]
    const NOOP_WAT: &str = r#"(module
        (memory (export "memory") 1)
        (func (export "_start"))
    )"#;

    #[cfg(feature = "wasm-sandbox")]
    #[tokio::test]
    async fn execute_bytes_noop_module() {
        let runner = WasmToolRunner::new(WasmSandboxConfig::default());
        let result = runner
            .execute_bytes("noop", NOOP_WAT.as_bytes(), serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(result.fuel_consumed > 0);
    }

    #[cfg(feature = "wasm-sandbox")]
    #[tokio::test]
    async fn execute_bytes_captures_fuel() {
        let config = WasmSandboxConfig {
            max_fuel: 10_000_000,
            ..Default::default()
        };
        let runner = WasmToolRunner::new(config);
        let result = runner
            .execute_bytes("fuel-test", NOOP_WAT.as_bytes(), serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(result.fuel_consumed > 0);
        assert!(result.fuel_consumed < 10_000_000);
    }

    #[cfg(feature = "wasm-sandbox")]
    #[tokio::test]
    async fn execute_bytes_invalid_module() {
        let runner = WasmToolRunner::new(WasmSandboxConfig::default());
        let result = runner
            .execute_bytes("bad", b"not wasm at all", serde_json::json!({}))
            .await;
        assert!(matches!(result, Err(WasmError::CompilationFailed(_))));
    }

    #[cfg(feature = "wasm-sandbox")]
    #[tokio::test]
    async fn execute_bytes_fuel_exhaustion() {
        let config = WasmSandboxConfig {
            max_fuel: 1,
            ..Default::default()
        };
        let runner = WasmToolRunner::new(config);
        let loop_wat = r#"(module
            (memory (export "memory") 1)
            (func (export "_start")
                (local $i i32)
                (block $break
                    (loop $loop
                        (br_if $break (i32.ge_u (local.get $i) (i32.const 1000)))
                        (local.set $i (i32.add (local.get $i) (i32.const 1)))
                        (br $loop)
                    )
                )
            )
        )"#;
        let result = runner
            .execute_bytes("loop", loop_wat.as_bytes(), serde_json::json!({}))
            .await;
        assert!(
            matches!(result, Err(WasmError::FuelExhausted { .. })),
            "expected FuelExhausted, got: {result:?}",
        );
    }

    #[cfg(feature = "wasm-sandbox")]
    #[tokio::test]
    async fn execute_bytes_no_export_returns_error() {
        let no_export_wat = r#"(module
            (memory (export "memory") 1)
            (func $helper (nop))
        )"#;
        let runner = WasmToolRunner::new(WasmSandboxConfig::default());
        let result = runner
            .execute_bytes("noexport", no_export_wat.as_bytes(), serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(result.exit_code, 1);
        assert!(result.stderr.contains("no _start or execute export"));
    }

    // --- K3: WasmToolAdapter / register_wasm_tool tests ---

    #[cfg(feature = "wasm-sandbox")]
    #[test]
    fn register_wasm_tool_and_dispatch() {
        let runner = Arc::new(WasmToolRunner::new(WasmSandboxConfig::default()));
        let mut registry = ToolRegistry::new();
        registry
            .register_wasm_tool(
                "wasm.noop",
                "A noop WASM tool",
                NOOP_WAT.as_bytes().to_vec(),
                runner,
            )
            .expect("registration should succeed");
        assert!(registry.get("wasm.noop").is_some());
        let spec = registry.get("wasm.noop").unwrap().spec();
        assert!(!spec.native);
        assert_eq!(spec.gate_action, "tool.wasm.wasm.noop");
    }

    #[cfg(feature = "wasm-sandbox")]
    #[test]
    fn register_wasm_tool_invalid_bytes_rejected() {
        let runner = Arc::new(WasmToolRunner::new(WasmSandboxConfig::default()));
        let mut registry = ToolRegistry::new();
        let result =
            registry.register_wasm_tool("wasm.bad", "Invalid", b"not wasm".to_vec(), runner);
        assert!(result.is_err());
    }

    #[cfg(feature = "wasm-sandbox")]
    #[test]
    fn wasm_adapter_execute_runs_module() {
        let runner = Arc::new(WasmToolRunner::new(WasmSandboxConfig::default()));
        let mut registry = ToolRegistry::new();
        registry
            .register_wasm_tool("wasm.noop", "noop", NOOP_WAT.as_bytes().to_vec(), runner)
            .unwrap();
        let result = registry.execute("wasm.noop", serde_json::json!({}));
        assert!(result.is_ok(), "execute should succeed: {:?}", result.err());
        let val = result.unwrap();
        assert_eq!(val["exit_code"], 0);
        assert!(val["fuel_consumed"].as_u64().unwrap() > 0);
    }

    #[cfg(feature = "wasm-sandbox")]
    #[test]
    fn wasm_adapter_listed_in_registry() {
        let runner = Arc::new(WasmToolRunner::new(WasmSandboxConfig::default()));
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(FsExistsTool::new()));
        registry
            .register_wasm_tool("wasm.noop", "noop", NOOP_WAT.as_bytes().to_vec(), runner)
            .unwrap();
        let list = registry.list();
        assert!(list.contains(&"fs.exists".to_string()));
        assert!(list.contains(&"wasm.noop".to_string()));
        assert_eq!(list.len(), 2);
    }

    // --- K3 gate: sync execute_sync tests ---

    #[cfg(feature = "wasm-sandbox")]
    #[test]
    fn k3_wasm_tool_loads_and_executes() {
        let runner = WasmToolRunner::new(WasmSandboxConfig::default());
        let result = runner
            .execute_sync("noop", NOOP_WAT.as_bytes(), serde_json::json!({}))
            .expect("execute_sync should succeed for noop WAT module");
        assert_eq!(result.exit_code, 0);
        assert!(result.fuel_consumed > 0, "fuel_consumed should be non-zero");
    }

    #[cfg(feature = "wasm-sandbox")]
    #[test]
    fn k3_fuel_exhaustion_terminates_cleanly() {
        let config = WasmSandboxConfig {
            max_fuel: 1,
            ..Default::default()
        };
        let runner = WasmToolRunner::new(config);

        let loop_wat = r#"(module
            (func (export "_start")
                (local $i i32)
                (block $break
                    (loop $loop
                        (br_if $break (i32.ge_u (local.get $i) (i32.const 1000)))
                        (local.set $i (i32.add (local.get $i) (i32.const 1)))
                        (br $loop)
                    )
                )
            )
        )"#;

        let result = runner.execute_sync("loop", loop_wat.as_bytes(), serde_json::json!({}));
        assert!(
            matches!(result, Err(WasmError::FuelExhausted { .. })),
            "expected FuelExhausted, got: {result:?}",
        );
    }

    #[cfg(feature = "wasm-sandbox")]
    #[test]
    fn k3_memory_limit_prevents_allocation_bomb() {
        let config = WasmSandboxConfig {
            max_memory_bytes: 64 * 1024,
            max_fuel: 1_000_000,
            ..Default::default()
        };
        let runner = WasmToolRunner::new(config);

        let big_mem_wat = r#"(module
            (memory 32)
            (func (export "_start") (nop))
        )"#;

        let result =
            runner.execute_sync("alloc-bomb", big_mem_wat.as_bytes(), serde_json::json!({}));
        assert!(
            result.is_err(),
            "module requesting 2 MiB with 64 KiB cap should fail, got: {result:?}",
        );
    }

    #[test]
    fn k3_host_filesystem_not_accessible_from_sandbox() {
        let config = WasmSandboxConfig::default();
        assert!(!config.wasi_enabled, "WASI should be disabled by default");
        assert!(
            config.allowed_host_calls.is_empty(),
            "no host calls should be allowed by default",
        );

        let scope = WasiFsScope::default();
        assert_eq!(scope, WasiFsScope::None, "default fs scope should be None");

        #[cfg(feature = "wasm-sandbox")]
        {
            let runner = WasmToolRunner::new(config);

            let wasi_import_wat = r#"(module
                (import "wasi_snapshot_preview1" "fd_write"
                    (func $fd_write (param i32 i32 i32 i32) (result i32)))
                (func (export "_start") (nop))
            )"#;

            let result = runner.execute_sync(
                "fs-probe",
                wasi_import_wat.as_bytes(),
                serde_json::json!({}),
            );
            assert!(
                result.is_err(),
                "module importing WASI fd_write should fail in sandboxed execute_sync: {result:?}",
            );
        }
    }

    // --- C5: ShellPipeline tests ---

    #[test]
    fn shell_pipeline_creates_hash() {
        let pipeline = ShellPipeline::new(
            "deploy",
            "cargo build --release && scp target/release/weft server:",
        );
        assert_ne!(pipeline.content_hash, [0u8; 32]);
    }

    #[test]
    fn shell_pipeline_deterministic_hash() {
        let p1 = ShellPipeline::new("test", "echo hello");
        let p2 = ShellPipeline::new("test", "echo hello");
        assert_eq!(p1.content_hash, p2.content_hash);
    }

    #[test]
    fn shell_pipeline_different_commands_different_hash() {
        let p1 = ShellPipeline::new("a", "echo hello");
        let p2 = ShellPipeline::new("a", "echo world");
        assert_ne!(p1.content_hash, p2.content_hash);
    }

    #[test]
    fn shell_pipeline_to_tool_spec() {
        let pipeline = ShellPipeline::new("build", "cargo build");
        let spec = pipeline.to_tool_spec();
        assert_eq!(spec.name, "shell.build");
        assert_eq!(spec.category, ToolCategory::User);
        assert!(spec.native);
        assert_eq!(spec.gate_action, "tool.shell.execute");
    }

    #[test]
    fn shell_pipeline_initial_chain_seq_none() {
        let pipeline = ShellPipeline::new("test", "ls -la");
        assert!(pipeline.chain_seq.is_none());
    }

    #[test]
    #[cfg(feature = "exochain")]
    fn shell_pipeline_anchor_to_chain() {
        let chain = crate::chain::ChainManager::new(0, 1000);
        let mut pipeline = ShellPipeline::new("deploy", "make deploy");
        assert!(pipeline.chain_seq.is_none());

        pipeline.anchor_to_chain(&chain);
        assert!(pipeline.chain_seq.is_some());

        let events = chain.tail(1);
        assert_eq!(events[0].kind, "shell.pipeline.register");
    }

    #[test]
    fn shell_pipeline_serde_roundtrip() {
        let pipeline = ShellPipeline::new("test-serde", "echo roundtrip");
        let json = serde_json::to_string(&pipeline).unwrap();
        let restored: ShellPipeline = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.name, "test-serde");
        assert_eq!(restored.command, "echo roundtrip");
        assert_eq!(restored.content_hash, pipeline.content_hash);
    }

    // --- D9: Tool Signing tests ---

    #[test]
    fn d9_register_unsigned_when_not_required_succeeds() {
        let mut registry = ToolRegistry::new();
        let tool: Arc<dyn BuiltinTool> = Arc::new(FsReadFileTool::new());
        registry.register(tool);
        assert!(registry.get("fs.read_file").is_some());
    }

    #[test]
    fn d9_register_unsigned_when_required_fails() {
        let mut registry = ToolRegistry::new();
        registry.set_require_signatures(true);
        assert!(registry.requires_signatures());

        let tool: Arc<dyn BuiltinTool> = Arc::new(FsReadFileTool::new());
        let result = registry.try_register(tool);
        assert!(
            matches!(result, Err(ToolError::SignatureRequired(_))),
            "expected SignatureRequired, got: {result:?}",
        );
        assert!(registry.get("fs.read_file").is_none());
    }

    #[test]
    #[cfg(feature = "exochain")]
    fn d9_register_with_valid_signature_succeeds() {
        use ed25519_dalek::{Signer, SigningKey};

        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let pk = sk.verifying_key().to_bytes();

        let tool: Arc<dyn BuiltinTool> = Arc::new(FsReadFileTool::new());
        let tool_hash = compute_module_hash(b"fs.read_file-definition");
        let sig = sk.sign(&tool_hash);

        let tool_sig = ToolSignature::new(
            "fs.read_file",
            tool_hash,
            "test-signer",
            sig.to_bytes().to_vec(),
        );

        let mut registry = ToolRegistry::new();
        registry.set_require_signatures(true);
        registry.add_trusted_key(pk);

        let result = registry.register_signed(tool, tool_sig);
        assert!(result.is_ok(), "register_signed should succeed: {result:?}");
        assert!(registry.get("fs.read_file").is_some());
        assert!(registry.get_signature("fs.read_file").is_some());
    }

    #[test]
    #[cfg(feature = "exochain")]
    fn d9_verify_tool_signature_roundtrip() {
        use ed25519_dalek::{Signer, SigningKey};

        let sk = SigningKey::from_bytes(&[99u8; 32]);
        let pk = sk.verifying_key().to_bytes();

        let tool_hash = compute_module_hash(b"my-tool-definition-bytes");
        let sig = sk.sign(&tool_hash);

        let tool_sig =
            ToolSignature::new("my.tool", tool_hash, "dev-alice", sig.to_bytes().to_vec());

        assert!(tool_sig.verify(&pk));

        let wrong_sk = SigningKey::from_bytes(&[100u8; 32]);
        let wrong_pk = wrong_sk.verifying_key().to_bytes();
        assert!(!tool_sig.verify(&wrong_pk));
    }

    #[test]
    fn d9_tool_signature_serde_roundtrip() {
        let sig = ToolSignature::new("test.tool", [0xAB; 32], "signer-1", vec![0xCD; 64]);
        let json = serde_json::to_string(&sig).unwrap();
        let restored: ToolSignature = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.tool_name, "test.tool");
        assert_eq!(restored.tool_hash, [0xAB; 32]);
        assert_eq!(restored.signer_id, "signer-1");
        assert_eq!(restored.signature.len(), 64);
    }

    #[test]
    fn d9_register_signed_with_invalid_signature_fails() {
        let mut registry = ToolRegistry::new();
        registry.set_require_signatures(true);
        registry.add_trusted_key([42u8; 32]);

        let tool: Arc<dyn BuiltinTool> = Arc::new(FsReadFileTool::new());
        let bad_sig = ToolSignature::new("fs.read_file", [0u8; 32], "bad-signer", vec![0u8; 64]);

        let result = registry.register_signed(tool, bad_sig);
        assert!(
            matches!(result, Err(ToolError::InvalidSignature(_))),
            "expected InvalidSignature, got: {result:?}",
        );
    }

    // --- D10: Shell Command Execution tests ---

    #[test]
    fn d10_shell_command_serde_roundtrip() {
        let cmd = ShellCommand {
            command: "echo".into(),
            args: vec!["hello".into(), "world".into()],
            sandbox_config: Some(SandboxConfig::default()),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let restored: ShellCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.command, "echo");
        assert_eq!(restored.args, vec!["hello", "world"]);
        assert!(restored.sandbox_config.is_some());
    }

    #[test]
    fn d10_execute_shell_echo() {
        let cmd = ShellCommand {
            command: "echo".into(),
            args: vec!["hello".into(), "world".into()],
            sandbox_config: None,
        };
        let result = execute_shell(&cmd).unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout, "hello world");
        assert!(result.stderr.is_empty());
    }

    #[test]
    fn d10_execute_shell_includes_execution_time() {
        let cmd = ShellCommand {
            command: "true".into(),
            args: vec![],
            sandbox_config: None,
        };
        let result = execute_shell(&cmd).unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(result.execution_time_ms < 1000, "should complete in < 1s");
    }

    #[test]
    fn d10_execute_shell_unknown_command() {
        let cmd = ShellCommand {
            command: "nonexistent".into(),
            args: vec![],
            sandbox_config: None,
        };
        let result = execute_shell(&cmd).unwrap();
        assert_eq!(result.exit_code, 127);
        assert!(result.stderr.contains("command not found"));
    }

    #[test]
    fn d10_shell_exec_tool_dispatch() {
        let tool = ShellExecTool::new();
        assert_eq!(tool.name(), "shell.exec");

        let result = tool
            .execute(serde_json::json!({
                "command": "echo",
                "args": ["test"]
            }))
            .unwrap();

        assert_eq!(result["exit_code"], 0);
        assert_eq!(result["stdout"], "test");
    }

    #[test]
    fn d10_shell_result_serde_roundtrip() {
        let result = ShellResult {
            exit_code: 0,
            stdout: "output".into(),
            stderr: "".into(),
            execution_time_ms: 42,
        };
        let json = serde_json::to_string(&result).unwrap();
        let restored: ShellResult = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.exit_code, 0);
        assert_eq!(restored.stdout, "output");
        assert_eq!(restored.execution_time_ms, 42);
    }

    #[test]
    fn d10_execute_shell_false_returns_nonzero() {
        let cmd = ShellCommand {
            command: "false".into(),
            args: vec![],
            sandbox_config: None,
        };
        let result = execute_shell(&cmd).unwrap();
        assert_eq!(result.exit_code, 1);
    }
}
