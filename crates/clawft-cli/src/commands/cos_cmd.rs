//! `weft cos serve`: the chief-of-staff agent host (ADR-118 C3), the same
//! entry point as the `weftos-cos` binary.

use clap::{Args, Subcommand};

#[derive(Args)]
pub struct CosArgs {
    #[command(subcommand)]
    pub action: CosAction,
}

#[derive(Subcommand)]
pub enum CosAction {
    /// Start the agent and its loopback conversation API (127.0.0.1:18300).
    Serve(weftos_cos::cli::ServeArgs),
}

pub async fn run(args: CosArgs) -> anyhow::Result<()> {
    match args.action {
        CosAction::Serve(serve) => weftos_cos::cli::serve(serve).await,
    }
}
