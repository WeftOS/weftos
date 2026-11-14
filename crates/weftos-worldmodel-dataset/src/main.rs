use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use weftos_worldmodel_dataset::{
    ArchiveRequest, ObjectStatus, StoreTarget, archive_session, verify_archive,
};

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
        /// Also upload to this store with `weaver store put` (off by default).
        #[arg(long, requires = "principal")]
        store: Option<String>,
        /// Key prefix: objects are <prefix>/<session>/<file>.
        #[arg(long, default_value = "", requires = "store")]
        store_prefix: String,
        /// Principal for the store upload (the weaver --as value). Never read from the environment.
        #[arg(long = "as", requires = "store")]
        principal: Option<String>,
        /// `weaver` binary to run; defaults to `weaver` on PATH.
        #[arg(long, requires = "store")]
        weaver_bin: Option<String>,
        /// Print the store commands without running them.
        #[arg(long, requires = "store")]
        store_dry_run: bool,
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
            store,
            store_prefix,
            principal,
            weaver_bin,
            store_dry_run,
        } => {
            let mut req = ArchiveRequest::new(session_dir, dest);
            req.session = session;
            req.protocol = protocol;
            req.consent = consent.map(serde_json::Value::String);
            req.overwrite = overwrite;
            if let (Some(name), Some(principal)) = (store, principal) {
                let mut t = StoreTarget::new(name, store_prefix, principal);
                t.weaver_bin = weaver_bin;
                t.dry_run = store_dry_run;
                req.store = Some(t);
            }
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
            if let Some(up) = &out.store_upload {
                for c in &up.commands {
                    println!("{}", c.join(" "));
                }
                for o in &up.objects {
                    if o.status != ObjectStatus::DryRun {
                        println!("store {:?} {}", o.status, o.key);
                    }
                    if let Some(e) = &o.stderr_tail {
                        eprintln!("store {}: {e}", o.key);
                    }
                }
                if !up.ok {
                    bail!("store upload failed; the filesystem copy is in place");
                }
            }
        }
        Cmd::Verify { dataset_dir } => {
            let v = verify_archive(&dataset_dir)?;
            for f in v.files.iter().filter(|f| !f.ok) {
                eprintln!("MISMATCH {}", f.name);
            }
            for p in &v.manifest_problems {
                eprintln!("MANIFEST {p}");
            }
            if !v.ok {
                bail!("verification failed");
            }
            println!("ok: {} files match the manifest", v.files.len());
        }
    }
    Ok(())
}
