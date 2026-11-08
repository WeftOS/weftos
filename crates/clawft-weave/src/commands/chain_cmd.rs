//! `weaver chain` — local chain management commands.

use clap::{Args, Subcommand};
use comfy_table::{Cell, Table};

use crate::protocol::{
    ChainEventInfo, ChainExportParams, ChainGetParams, ChainLocalParams, ChainStatusResult, ChainVerifyResult,
    Request,
};

#[derive(Args)]
pub struct ChainArgs {
    #[command(subcommand)]
    pub command: ChainCommand,
}

#[derive(Subcommand)]
pub enum ChainCommand {
    /// Show local chain status.
    Status,
    /// List recent chain events.
    Local {
        /// Number of events to show (default: 20).
        #[arg(short, long, default_value_t = 20)]
        count: usize,
    },
    /// Create a chain checkpoint.
    Checkpoint,
    /// Verify chain integrity (hash linking).
    Verify,
    /// Show one event in full with its checkpoint-signature coverage.
    Get {
        /// Sequence number of the event.
        sequence: u64,
        /// Leave the hex signatures out of the coverage.
        #[arg(long)]
        no_signatures: bool,
    },
    /// Export chain to file (JSON or RVF format).
    Export {
        /// Output format: json or rvf.
        #[arg(short, long, default_value = "json")]
        format: String,
        /// Output file path. JSON defaults to stdout; RVF defaults to daemon-side export.
        #[arg(short, long)]
        output: Option<String>,
    },
}

pub async fn run(args: ChainArgs) -> anyhow::Result<()> {
    let mut client = clawft_rpc::connect_or_bail().await?;

    match args.command {
        ChainCommand::Status => {
            let resp = client.simple_call("chain.status").await?;
            if !resp.ok {
                anyhow::bail!("{}", resp.error.unwrap_or_default());
            }
            let status: ChainStatusResult =
                serde_json::from_value(resp.result.unwrap_or_default())?;

            println!("Chain Status");
            println!("  Chain ID:                {}", status.chain_id);
            println!("  Sequence:                {}", status.sequence);
            println!("  Events:                  {}", status.event_count);
            println!("  Checkpoints:             {}", status.checkpoint_count);
            println!(
                "  Since last checkpoint:   {}",
                status.events_since_checkpoint
            );
            println!("  Last hash:               {}...", &status.last_hash[..16]);
        }
        ChainCommand::Local { count } => {
            let params = serde_json::to_value(ChainLocalParams { count })?;
            let resp = client
                .call(Request::with_params("chain.local", params))
                .await?;
            if !resp.ok {
                anyhow::bail!("{}", resp.error.unwrap_or_default());
            }
            let events: Vec<ChainEventInfo> =
                serde_json::from_value(resp.result.unwrap_or_default())?;

            let mut table = Table::new();
            table.set_header(vec!["Seq", "Source", "Kind", "Detail", "Timestamp", "Hash"]);
            for e in &events {
                let detail = if e.detail.len() > 40 {
                    format!("{}...", &e.detail[..40])
                } else {
                    e.detail.clone()
                };
                table.add_row(vec![
                    Cell::new(e.sequence),
                    Cell::new(&e.source),
                    Cell::new(&e.kind),
                    Cell::new(detail),
                    Cell::new(&e.timestamp[..19]),
                    Cell::new(format!("{}...", &e.hash[..12])),
                ]);
            }
            println!("{table}");
        }
        ChainCommand::Checkpoint => {
            let resp = client.simple_call("chain.checkpoint").await?;
            if !resp.ok {
                anyhow::bail!("{}", resp.error.unwrap_or_default());
            }
            println!("Checkpoint created.");
            if let Some(result) = resp.result {
                if let Some(seq) = result.get("sequence") {
                    println!("  Sequence: {seq}");
                }
                match result.get("signed").and_then(|v| v.as_bool()) {
                    Some(true) => println!("  Signed:   yes ({})", result.get("rvf_path").and_then(|v| v.as_str()).unwrap_or("")),
                    _ => println!("  Signed:   no (chain has no signing key or no checkpoint path)"),
                }
            }
        }
        ChainCommand::Verify => {
            let resp = client.simple_call("chain.verify").await?;
            if !resp.ok {
                anyhow::bail!("{}", resp.error.unwrap_or_default());
            }
            let result: ChainVerifyResult =
                serde_json::from_value(resp.result.unwrap_or_default())?;

            if result.valid {
                println!("Chain integrity: VALID");
            } else {
                println!("Chain integrity: INVALID");
            }
            println!("Events verified: {}", result.event_count);
            match result.signature_verified {
                Some(true) => println!(
                    "Signature:       VALID through sequence {} ({})",
                    result.signed_through.map(|s| s.to_string()).unwrap_or_default(),
                    if result.pq_verified == Some(true) { "Ed25519 + ML-DSA-65" } else { "Ed25519" },
                ),
                Some(false) => println!("Signature:       INVALID"),
                None => println!("Signature:       unsigned (run `weaver chain checkpoint`)"),
            }
            println!("Errors: {}", result.errors.len());
            for err in &result.errors {
                println!("  - {err}");
            }
        }
        ChainCommand::Get { sequence, no_signatures } => {
            let params = serde_json::to_value(ChainGetParams { sequence, signatures: !no_signatures })?;
            let resp = client.call(Request::with_params("chain.get", params)).await?;
            if !resp.ok {
                anyhow::bail!("{}", resp.error.unwrap_or_default());
            }
            println!("{}", serde_json::to_string_pretty(&resp.result.unwrap_or_default())?);
        }
        ChainCommand::Export { format, output } => {
            let params = serde_json::to_value(ChainExportParams {
                format: format.clone(),
                output: output.clone(),
            })?;
            let resp = client
                .call(Request::with_params("chain.export", params))
                .await?;
            if !resp.ok {
                anyhow::bail!("{}", resp.error.unwrap_or_default());
            }

            match format.as_str() {
                "rvf" => {
                    let result = resp.result.unwrap_or_default();
                    let path = result["path"].as_str().unwrap_or("(unknown)");
                    println!("Chain exported to RVF: {path}");
                }
                _ => {
                    let result = resp.result.unwrap_or_default();
                    if let Some(ref output_path) = output {
                        let json = serde_json::to_string_pretty(&result)?;
                        std::fs::write(output_path, json)?;
                        println!("Chain exported to JSON: {output_path}");
                    } else {
                        let json = serde_json::to_string_pretty(&result)?;
                        println!("{json}");
                    }
                }
            }
        }
    }

    Ok(())
}
