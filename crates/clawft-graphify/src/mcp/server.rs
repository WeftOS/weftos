//! stdio MCP server shell for graphify (WEFT-369).
//!
//! Implements JSON-RPC 2.0 over newline-delimited streams — same shape as
//! [`clawft_services::mcp::server::McpServerShell`], but self-contained so
//! `clawft-graphify` does not depend on clawft-services.

use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use super::MCP_PROTOCOL_VERSION;
use super::SERVER_NAME;
use super::handlers::{CallResult, McpConfig, dispatch_tool};
use super::tools::tool_definitions;

const METHOD_NOT_FOUND: i32 = -32601;
const NOT_INITIALIZED: i32 = -32002;
const INVALID_REQUEST: i32 = -32600;

/// Run the MCP server on stdin/stdout until EOF.
pub async fn serve_stdio(config: McpConfig) -> std::io::Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let reader = BufReader::new(stdin);
    serve(config, reader, stdout).await
}

/// Run the MCP server loop on arbitrary async reader/writer (tests + stdio).
pub async fn serve<R, W>(config: McpConfig, reader: R, mut writer: W) -> std::io::Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut lines = reader.lines();
    let mut initialized = false;

    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                write_json(
                    &mut writer,
                    &error_response(Value::Null, INVALID_REQUEST, "Parse error"),
                )
                .await?;
                continue;
            }
        };

        let method = msg.get("method").and_then(|v| v.as_str()).unwrap_or("");
        let id = msg.get("id").cloned();
        let params = msg
            .get("params")
            .cloned()
            .unwrap_or_else(|| Value::Object(Default::default()));
        let is_notification = id.is_none();

        match method {
            "initialize" => {
                initialized = true;
                let result = json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {
                        "tools": { "listChanged": false }
                    },
                    "serverInfo": {
                        "name": SERVER_NAME,
                        "version": env!("CARGO_PKG_VERSION")
                    }
                });
                if let Some(id) = id {
                    write_json(&mut writer, &success_response(id, result)).await?;
                }
            }
            "notifications/initialized" | "initialized" => {
                // Notification — no response.
            }
            "ping" => {
                if let Some(id) = id {
                    write_json(&mut writer, &success_response(id, json!({}))).await?;
                }
            }
            _ if !initialized => {
                if !is_notification {
                    write_json(
                        &mut writer,
                        &error_response(
                            id.unwrap_or(Value::Null),
                            NOT_INITIALIZED,
                            "Server not initialized",
                        ),
                    )
                    .await?;
                }
            }
            "tools/list" => {
                let tools = tool_definitions();
                let result = json!({ "tools": tools });
                if let Some(id) = id {
                    write_json(&mut writer, &success_response(id, result)).await?;
                }
            }
            "tools/call" => {
                let name = params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| Value::Object(Default::default()));

                let CallResult { text, is_error } = dispatch_tool(&config, &name, &args);
                let result = json!({
                    "content": [{ "type": "text", "text": text }],
                    "isError": is_error
                });
                if let Some(id) = id {
                    write_json(&mut writer, &success_response(id, result)).await?;
                }
            }
            _ => {
                if !is_notification {
                    write_json(
                        &mut writer,
                        &error_response(
                            id.unwrap_or(Value::Null),
                            METHOD_NOT_FOUND,
                            &format!("Method not found: {method}"),
                        ),
                    )
                    .await?;
                }
            }
        }
    }

    Ok(())
}

fn success_response(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    })
}

fn error_response(id: Value, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message
        }
    })
}

async fn write_json<W: AsyncWrite + Unpin>(writer: &mut W, value: &Value) -> std::io::Result<()> {
    let mut line = serde_json::to_string(value).map_err(std::io::Error::other)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Integration-style protocol tests (in-memory duplex)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::build_from_json;
    use crate::export::json as export_json;
    use std::io::Cursor;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn request_line(id: u64, method: &str, params: Value) -> String {
        format!(
            "{}\n",
            serde_json::to_string(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            }))
            .unwrap()
        )
    }

    fn parse_responses(output: &[u8]) -> Vec<Value> {
        String::from_utf8_lossy(output)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("json response"))
            .collect()
    }

    async fn run_session(input: &str, config: McpConfig) -> Vec<Value> {
        let reader = Cursor::new(input.as_bytes().to_vec());
        let mut out: Vec<u8> = Vec::new();
        serve(config, reader, &mut out).await.expect("serve");
        parse_responses(&out)
    }

    #[tokio::test]
    async fn initialize_and_list_tools() {
        let input = format!(
            "{}{}",
            request_line(
                1,
                "initialize",
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": { "name": "test", "version": "0" }
                })
            ),
            request_line(2, "tools/list", json!({}))
        );
        let responses = run_session(&input, McpConfig::default()).await;
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0]["result"]["serverInfo"]["name"], SERVER_NAME);
        let tools = responses[1]["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 4);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"graphify_query"));
        assert!(names.contains(&"graphify_ingest"));
        assert!(names.contains(&"graphify_export"));
        assert!(names.contains(&"graphify_diff"));
    }

    #[tokio::test]
    async fn rejects_tools_before_initialize() {
        let input = request_line(1, "tools/list", json!({}));
        let responses = run_session(&input, McpConfig::default()).await;
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0]["error"]["code"], NOT_INITIALIZED);
    }

    #[tokio::test]
    async fn tools_call_query() {
        let tmp = TempDir::new().unwrap();
        let graph_path = tmp.path().join("graph.json");
        let sample = json!({
            "nodes": [{
                "id": "n1",
                "label": "KernelRouter",
                "source_file": "kernel/router.rs",
                "file_type": "code",
                "entity_type": "module"
            }],
            "edges": []
        });
        let kg = build_from_json(&sample).unwrap();
        export_json::to_json(&kg, &graph_path).unwrap();

        let cfg = McpConfig {
            work_dir: tmp.path().to_path_buf(),
            default_graph: PathBuf::from("graph.json"),
        };

        let init = request_line(
            1,
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0" }
            }),
        );
        let notif = format!(
            "{}\n",
            serde_json::to_string(&json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized",
                "params": {}
            }))
            .unwrap()
        );
        let call = request_line(
            3,
            "tools/call",
            json!({
                "name": "graphify_query",
                "arguments": {
                    "question": "KernelRouter",
                    "graph": "graph.json",
                    "mode": "none"
                }
            }),
        );
        let input = format!("{init}{notif}{call}");

        let responses = run_session(&input, cfg).await;
        // initialize + tools/call (notification has no response)
        assert_eq!(responses.len(), 2);
        let call_resp = &responses[1];
        assert_eq!(call_resp["id"], 3);
        assert_eq!(call_resp["result"]["isError"], false);
        let text = call_resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("KernelRouter"), "{text}");
    }
}
