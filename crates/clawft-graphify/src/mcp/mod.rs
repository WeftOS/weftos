//! Graphify MCP server (WEFT-369 / Phase 6).
//!
//! Exposes knowledge-graph capabilities over the Model Context Protocol
//! (JSON-RPC 2.0, newline-delimited stdio). Ports the Python
//! `graphify/serve.py` surface onto the Rust crate, reusing the existing
//! extract / query / export / diff paths.
//!
//! # Tools
//!
//! | Tool | Description |
//! |------|-------------|
//! | `graphify_query` | Keyword + BFS/DFS search over a graph JSON |
//! | `graphify_ingest` | Ingest a local directory (or URL with `http-client`) |
//! | `graphify_export` | Export graph JSON to a supported format |
//! | `graphify_diff` | Diff two graph JSON snapshots |
//!
//! # Run
//!
//! ```text
//! weaver graphify serve
//! # or from library code:
//! // clawft_graphify::mcp::serve_stdio(McpConfig::default()).await?;
//! ```
//!
//! Feature flag: enable with `clawft-graphify` feature `mcp` (on by default
//! when using the weaver binary's graphify features).

mod handlers;
mod server;
mod tools;

pub use handlers::{
    CallResult, McpConfig, dispatch_tool, export_graph, graph_diff_paths, ingest_target,
    query_graph,
};
pub use server::{serve, serve_stdio};
pub use tools::{TOOL_NAMES, tool_definitions};

/// MCP protocol version advertised during `initialize`.
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

/// Server product name in `serverInfo`.
pub const SERVER_NAME: &str = "clawft-graphify";
