//! `weftos team apply|status|uninstall`: project-scoped team apply (ADR-112
//! TM2) over `weftos::init::team`, the same path the daemon's `agent.install`
//! action runs.

use std::path::PathBuf;

use clap::{Args, Subcommand};
use weftos::init::Host;
use weftos::init::birth::format_birth;
use weftos::init::digest::Subject;
use weftos::init::team::{
    ApplyRequest, TeamError, apply_request, format_collisions, format_status, status, uninstall,
};

#[derive(Args)]
pub struct Hosts {
    /// Render for Claude Code (default when no host flag is given)
    #[arg(long)]
    claude: bool,
    /// Render for Grok Build
    #[arg(long)]
    grok: bool,
    /// Render for Codex
    #[arg(long)]
    codex: bool,
}

#[derive(Subcommand)]
pub enum TeamCmd {
    /// Apply a team (or one agent package) to a project
    ///
    /// Writes only inside the project, records .weftos/agents.lock.json and
    /// refuses, writing nothing, if a target file was changed locally or
    /// belongs to something else. Inactive members are never applied.
    Apply {
        /// Team name (agents/teams/<name>), or a package id with --package
        name: String,
        /// Project directory
        #[arg(long, default_value = ".")]
        project: PathBuf,
        /// NAME is an agent package, not a team
        #[arg(long)]
        package: bool,
        #[command(flatten)]
        hosts: Hosts,
        /// Use an agents/ source tree on disk instead of the embedded one
        #[arg(long, value_name = "DIR")]
        from: Option<PathBuf>,
        /// Refuse unless the bundle digest is this (sha256:<hex>)
        #[arg(long, value_name = "DIGEST")]
        pin: Option<String>,
        /// Report what would happen; write nothing
        #[arg(long)]
        plan: bool,
        /// Print the birth report as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show what is applied to a project and whether the files still match
    Status {
        #[arg(long, default_value = ".")]
        project: PathBuf,
        #[arg(long, value_name = "DIR")]
        from: Option<PathBuf>,
    },
    /// Remove exactly what the lock lists (one team, or all of it)
    Uninstall {
        /// Team to remove (default: everything the lock lists)
        name: Option<String>,
        #[arg(long, default_value = ".")]
        project: PathBuf,
        #[arg(long, value_name = "DIR")]
        from: Option<PathBuf>,
        /// Report what would be removed; write nothing
        #[arg(long)]
        plan: bool,
    },
}

fn hosts(h: &Hosts) -> Vec<Host> {
    let v: Vec<Host> = [(h.claude, Host::Claude), (h.grok, Host::Grok), (h.codex, Host::Codex)]
        .into_iter()
        .filter_map(|(on, x)| on.then_some(x))
        .collect();
    if v.is_empty() { vec![Host::Claude] } else { v }
}

fn report(e: &TeamError) -> i32 {
    eprintln!("weftos team: {e}");
    if !e.collisions.is_empty() {
        eprint!("{}", format_collisions(&e.collisions));
    }
    if e.refused { 2 } else { 1 }
}

pub fn run(cmd: TeamCmd) -> i32 {
    match cmd {
        TeamCmd::Apply { name, project, package, hosts: h, from, pin, plan, json } => {
            let req = ApplyRequest {
                root: project,
                subject: if package { Subject::Agent } else { Subject::Team },
                name,
                hosts: hosts(&h),
                from,
                pin: pin.map(|p| p.strip_prefix("sha256:").unwrap_or(&p).to_string()),
                dry_run: plan,
            };
            match apply_request(&req) {
                Ok(a) if json => {
                    println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                        "digest": a.digest, "already_applied": a.already, "files": a.touched,
                        "collisions": a.collisions, "birth": a.birth,
                    })).unwrap_or_default());
                    0
                }
                Ok(a) => {
                    print!("{}", format_collisions(&a.collisions));
                    if a.already {
                        println!("No changes: already applied (sha256:{}).", a.digest);
                    } else if plan {
                        println!("Plan only; re-run without --plan to write.");
                    } else {
                        println!("Applied sha256:{} ({} path(s) written).", a.digest, a.touched);
                    }
                    print!("{}", format_birth(&a.birth));
                    0
                }
                Err(e) => report(&e),
            }
        }
        TeamCmd::Status { project, from } => match status(&project, &from) {
            Ok(s) => {
                print!("{}", format_status(&s));
                0
            }
            Err(e) => report(&e),
        },
        TeamCmd::Uninstall { name, project, from, plan } => {
            match uninstall(&project, name.as_deref(), &from, plan) {
                Ok(u) => {
                    println!(
                        "{} {} package(s): {}",
                        if plan { "Would remove" } else { "Removed" },
                        u.removed.len(),
                        u.removed.join(", ")
                    );
                    for p in &u.kept {
                        println!("  kept (changed since apply): {p}");
                    }
                    0
                }
                Err(e) => report(&e),
            }
        }
    }
}
