//! Run evidence: what an instance did, captured under its output limit.
//!
//! Stdout is evidence and logs, never control input (ADR-100 section 4).
//! Chain payloads carry [`RunEvidence::audit`], which has counts and hashes
//! but no output content.

use serde::{Deserialize, Serialize};

/// A byte capture that keeps the first `limit` bytes and counts the rest.
#[derive(Debug, Clone)]
pub struct Capture {
    buf: Vec<u8>,
    total: u64,
    limit: usize,
}

impl Capture {
    /// Empty capture with a byte limit.
    pub fn new(limit: usize) -> Self {
        Self {
            buf: Vec::new(),
            total: 0,
            limit,
        }
    }

    /// Append a chunk.
    pub fn push(&mut self, chunk: &[u8]) {
        self.total += chunk.len() as u64;
        let room = self.limit.saturating_sub(self.buf.len());
        self.buf.extend_from_slice(&chunk[..chunk.len().min(room)]);
    }

    /// Captured bytes, lossily decoded.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.buf).into_owned()
    }

    /// Total bytes seen.
    pub fn total(&self) -> u64 {
        self.total
    }

    /// Whether bytes were dropped.
    pub fn truncated(&self) -> bool {
        self.total > self.buf.len() as u64
    }
}

/// What one run (or one stopped instance) produced.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RunEvidence {
    /// Adapter id.
    pub runtime: String,
    /// Instance id.
    pub instance_id: String,
    /// Arguments passed (binary path omitted).
    pub args: Vec<String>,
    /// Exit code, when the process exited normally.
    pub exit_code: Option<i32>,
    /// Terminating signal, when killed.
    pub signal: Option<i32>,
    /// Killed because it exceeded `[console].max_runtime_secs`.
    pub killed_for_timeout: bool,
    /// Stopped because its resident memory exceeded `[resources].ram_mb`.
    #[serde(default)]
    pub killed_for_memory: bool,
    /// Wall time.
    pub elapsed_ms: u64,
    /// Captured stdout (capped).
    pub stdout: String,
    /// Captured stderr (capped).
    pub stderr: String,
    /// Total stdout bytes produced.
    pub stdout_bytes: u64,
    /// Total stderr bytes produced.
    pub stderr_bytes: u64,
    /// Whether either stream exceeded the output limit.
    pub truncated: bool,
    /// Instances stopped to free the sensor feed before a console run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stopped_for_console: Vec<String>,
}

impl RunEvidence {
    /// Fill output fields from captures.
    pub fn with_output(mut self, out: &Capture, err: &Capture) -> Self {
        self.stdout = out.text();
        self.stderr = err.text();
        self.stdout_bytes = out.total();
        self.stderr_bytes = err.total();
        self.truncated = out.truncated() || err.truncated();
        self
    }

    /// Stdout lines that parse as JSON objects (a cog's report lines).
    pub fn json_lines(&self) -> Vec<serde_json::Value> {
        self.stdout
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
            .filter(|v| v.is_object())
            .collect()
    }

    /// Exited 0 and was not killed.
    pub fn succeeded(&self) -> bool {
        self.exit_code == Some(0) && !self.killed_for_timeout && !self.killed_for_memory
    }

    /// Chain-safe summary: no output content.
    pub fn audit(&self) -> serde_json::Value {
        serde_json::json!({
            "runtime": self.runtime,
            "instance_id": self.instance_id,
            "exit_code": self.exit_code,
            "signal": self.signal,
            "killed_for_timeout": self.killed_for_timeout,
            "killed_for_memory": self.killed_for_memory,
            "elapsed_ms": self.elapsed_ms,
            "stdout_bytes": self.stdout_bytes,
            "stderr_bytes": self.stderr_bytes,
            "truncated": self.truncated,
            "stopped_for_console": self.stopped_for_console,
            "json_lines": self.json_lines().len(),
            "stdout_blake3": blake3::hash(self.stdout.as_bytes()).to_hex().to_string(),
        })
    }
}
