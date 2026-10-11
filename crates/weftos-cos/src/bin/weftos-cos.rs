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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env().add_directive("weftos_cos=info".parse()?))
        .with_writer(std::io::stderr)
        .init();
    match Cli::parse().cmd {
        Cmd::Serve(args) => weftos_cos::cli::serve(args).await,
    }
}
