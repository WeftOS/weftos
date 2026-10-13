//! A tiny stdio MCP server for trying `mcp_servers` without an account.
//!
//! Tools: `echo` (returns its text), `time.now` (UTC), `note.append` (keeps a
//! note in memory only; use it to see a gated write), `big` (a long result,
//! for output truncation). Newline-delimited JSON-RPC on stdin/stdout.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

fn tools() -> Value {
    json!([
        {"name": "echo", "description": "Return the given text unchanged.",
         "inputSchema": {"type": "object", "properties": {"text": {"type": "string", "description": "Text to echo"}}, "required": ["text"]}},
        {"name": "time.now", "description": "The current UTC time as RFC 3339.",
         "inputSchema": {"type": "object", "properties": {}}},
        {"name": "note.append", "description": "Append a note (kept in memory only). A write.",
         "inputSchema": {"type": "object", "properties": {"note": {"type": "string"}}, "required": ["note"]}},
        {"name": "big", "description": "Return `bytes` characters of filler.",
         "inputSchema": {"type": "object", "properties": {"bytes": {"type": "integer", "minimum": 1}}, "required": ["bytes"]}}
    ])
}

fn call(name: &str, args: &Value) -> Value {
    let text = |t: String| json!({"content": [{"type": "text", "text": t}]});
    match name {
        "echo" => text(args["text"].as_str().unwrap_or("").to_string()),
        "time.now" => text(chrono::Utc::now().to_rfc3339()),
        "note.append" => text(format!("noted: {}", args["note"].as_str().unwrap_or(""))),
        "big" => text("x".repeat(args["bytes"].as_u64().unwrap_or(1) as usize)),
        other => json!({"isError": true, "content": [{"type": "text", "text": format!("unknown tool {other}")}]}),
    }
}

fn main() {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines().map_while(Result::ok) {
        let Ok(req) = serde_json::from_str::<Value>(&line) else { continue };
        let Some(id) = req.get("id").cloned() else { continue }; // notification
        let result = match req["method"].as_str().unwrap_or("") {
            "initialize" => json!({"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "weftos-cos-mcp-echo", "version": "1"}}),
            "tools/list" => json!({"tools": tools()}),
            "tools/call" => call(req["params"]["name"].as_str().unwrap_or(""), &req["params"]["arguments"]),
            "ping" => json!({}),
            _ => {
                let err = json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not found"}});
                let _ = writeln!(out, "{err}");
                let _ = out.flush();
                continue;
            }
        };
        let _ = writeln!(out, "{}", json!({"jsonrpc": "2.0", "id": id, "result": result}));
        let _ = out.flush();
    }
}
