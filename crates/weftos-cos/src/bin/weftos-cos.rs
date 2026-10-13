//! `weftos-cos serve`: the chief-of-staff agent host (ADR-118 C3).

use clap::{Parser, Subcommand};
use weftos_cos::cli::ServeArgs;

#[derive(Parser)]
#[command(name = "weftos-cos", about = "Chief-of-staff agent host (ADR-118 C3)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start the agent and the conversation API.
    Serve(ServeArgs),
    /// Relay the hosted dashboard's phone page to this agent (and its turns back).
    Bridge(weftos_cos::bridge::BridgeArgs),
    /// Serve the chief of staff as an MCP server on stdio (drives a running agent over cos.v1).
    Mcp(weftos_cos::mcp_server::McpArgs),
    /// Check, start or stop the local model server.
    #[command(subcommand)]
    Model(weftos_cos::cli::ModelCmd),
    /// Create or list remote endpoints (tokens for non-loopback clients).
    #[command(subcommand)]
    Endpoint(weftos_cos::cli::EndpointCmd),
    /// Mirror the agent's state to an e-ink glance panel over USB serial (ADR-119 S4).
    GlanceSerial(weftos_cos::glance::GlanceSerialArgs),
    /// Transcribe a WAV file with the configured STT (no microphone needed).
    Transcribe {
        wav: std::path::PathBuf,
        #[arg(long)]
        config: Option<std::path::PathBuf>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env().add_directive("weftos_cos=info".parse()?))
        .with_writer(std::io::stderr)
        .init();
    match Cli::parse().cmd {
        Cmd::Serve(args) => weftos_cos::cli::serve(args).await,
        Cmd::Bridge(args) => weftos_cos::bridge::run(args).await,
        Cmd::Mcp(args) => weftos_cos::mcp_server::run(args).await,
        Cmd::Model(cmd) => weftos_cos::cli::model(cmd).await,
        Cmd::Endpoint(cmd) => weftos_cos::cli::endpoint(cmd),
        Cmd::GlanceSerial(args) => weftos_cos::glance::run(args).await,
        Cmd::Transcribe { wav, config } => weftos_cos::cli::transcribe(wav, config).await,
    }
}
