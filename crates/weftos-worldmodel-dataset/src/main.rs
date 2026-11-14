use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use weftos_worldmodel_dataset::{ArchiveRequest, archive_session, verify_archive};

#[derive(Parser)]
#[command(
    name = "weftos-worldmodel-dataset",
    version,
    about = "Archive a finished sensor session as a checksummed dataset"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Validate, build the manifest, copy and verify a session.
    Archive {
        /// Directory holding the session files.
        session_dir: PathBuf,
        /// Destination root (the dataset lands under ruview-demo/datasets/<session>).
        #[arg(long)]
        dest: PathBuf,
        /// Session id; inferred from the evidence file name when omitted.
        #[arg(long)]
        session: Option<String>,
        /// Protocol text, used only when the session has no protocol file.
        #[arg(long)]
        protocol: Option<String>,
        /// Consent statement, used only when the session has no consent.json.
        #[arg(long)]
        consent: Option<String>,
        /// Replace an existing archived session.
        #[arg(long)]
        overwrite: bool,
    },
    /// Re-check an archived dataset directory against its manifest.
    Verify { dataset_dir: PathBuf },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Archive {
            session_dir,
            dest,
            session,
            protocol,
            consent,
            overwrite,
        } => {
            let mut req = ArchiveRequest::new(session_dir, dest);
            req.session = session;
            req.protocol = protocol;
            req.consent = consent.map(serde_json::Value::String);
            req.overwrite = overwrite;
            let out = archive_session(&req)?;
            println!(
                "archived {} ({} files, {} evidence lines) to {}",
                out.manifest.session,
                out.manifest.files.len(),
                out.manifest.evidence.lines,
                out.dest_dir.display()
            );
            if !out.manifest.missing.is_empty() {
                println!(
                    "missing optional files: {}",
                    out.manifest.missing.join(", ")
                );
            }
        }
        Cmd::Verify { dataset_dir } => {
            let v = verify_archive(&dataset_dir)?;
            for f in v.files.iter().filter(|f| !f.ok) {
                eprintln!("MISMATCH {}", f.name);
            }
            if !v.ok {
                bail!("verification failed");
            }
            println!("ok: {} files match the manifest", v.files.len());
        }
    }
    Ok(())
}
