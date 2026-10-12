//! `weaver project` (ADR-103 A6, Phase 2 package G): operator commands for
//! per-project kernels. `weft project` keeps the registration commands.

use clap::{Parser, Subcommand};

use crate::project_migrate::{self, Action, MigrateError};

/// Project operator subcommand.
#[derive(Parser)]
#[command(about = "Projects on this machine: add, up, down, status, remove; kernel migration; anchor reset")]
pub struct ProjectArgs {
    /// Project subcommand.
    #[command(subcommand)]
    pub action: ProjectAction,
}

/// Project subcommands.
#[derive(Subcommand)]
pub enum ProjectAction {
    /// Opt one project into a supervised child kernel (or back out).
    ///
    /// Refuses while the project's own daemon runs (it is never signalled),
    /// copies workloads.json and apps.json into <root>/.weftos/state/,
    /// sets `[serve] via = "child-kernel"` and prints the rollback line.
    #[command(name = "migrate-kernel")]
    MigrateKernel {
        /// Project id or unique registered name.
        project: String,
        /// Print what would change; change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Set the project back to the user daemon (copies are left).
        #[arg(long)]
        revert: bool,
    },
    /// Add a project to this machine in one step: fetch it, register it, detect
    /// how it runs, install its router route, and say what to do next.
    ///
    /// SOURCE is `weftos://<mesh>/projects/<ULID>` (over the mesh, from the
    /// paired primary) or a git URL. Never copies `.env` files or values; lists
    /// the environment variable names the project asks for.
    Add(super::project_add_cmd::AddOpts),
    /// Start a project added here (process-compose, detached).
    Up {
        /// The project's slug.
        slug: String,
    },
    /// Stop a project added here.
    Down {
        /// The project's slug.
        slug: String,
    },
    /// Show processes, route, local URL and tailnet URL (every added project when no slug).
    Status {
        /// The project's slug.
        slug: Option<String>,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Undo what `add` did: stop it, drop its route, unregister it. Keeps the checkout.
    Remove {
        /// The project's slug.
        slug: String,
        /// Also delete the checkout (refuses uncommitted changes).
        #[arg(long)]
        purge: bool,
        /// With --purge: delete even with uncommitted changes.
        #[arg(long, requires = "purge")]
        force: bool,
    },
    /// Anchor operations on the user daemon (owner only).
    Anchor {
        /// Anchor subcommand.
        #[command(subcommand)]
        action: AnchorAction,
    },
}

/// `weaver project anchor` subcommands.
#[derive(Subcommand)]
pub enum AnchorAction {
    /// Retire a project's anchor head after its chain was reset on purpose.
    ///
    /// A project whose chain was moved aside restarts its anchors at seq 1,
    /// which the user daemon refuses while it holds the old head. This records
    /// a signed reset on the user chain (the old statements stay as history)
    /// and lets the project anchor again from genesis. Run it only when the
    /// project chain really was reset; it changes nothing about the project's
    /// key. Needs the owner's local socket (Admin); a token is refused.
    Reset {
        /// Project id (a ULID).
        #[arg(long)]
        project: String,
        /// Why (kept on the chain; control characters are removed).
        #[arg(long, default_value = "")]
        reason: String,
    },
}

/// Run the project subcommand.
pub async fn run(args: ProjectArgs) -> anyhow::Result<()> {
    match args.action {
        ProjectAction::MigrateKernel { project, dry_run, revert } => migrate(&project, dry_run, revert),
        ProjectAction::Add(o) => super::project_add_cmd::add(o).await,
        ProjectAction::Up { slug } => super::project_add_cmd::up(&slug, &super::project_add_cmd::pc_bin()).await,
        ProjectAction::Down { slug } => super::project_add_cmd::down(&slug, &super::project_add_cmd::pc_bin()).await,
        ProjectAction::Status { slug, json } => super::project_add_cmd::status(slug.as_deref(), json).await,
        ProjectAction::Remove { slug, purge, force } => super::project_add_cmd::remove(&slug, purge, force, &super::project_add_cmd::pc_bin()).await,
        ProjectAction::Anchor { action: AnchorAction::Reset { project, reason } } => anchor_reset(&project, &reason).await,
    }
}

async fn anchor_reset(project: &str, reason: &str) -> anyhow::Result<()> {
    let mut client = clawft_rpc::DaemonClient::connect()
        .await
        .ok_or_else(|| anyhow::anyhow!("no user daemon is running; start it with `weaver kernel start --profile user`"))?;
    let req = clawft_rpc::Request::with_params(
        "project.anchor.reset",
        serde_json::json!({ "project_id": project, "reason": reason }),
    );
    let resp = client.call(req).await?;
    if !resp.ok {
        anyhow::bail!("{}", resp.error.unwrap_or_else(|| "anchor reset refused".into()));
    }
    let r = resp.result.unwrap_or_default();
    println!(
        "project {project}: anchor epoch {} (retired head seq {}); its next anchor starts from seq 1",
        r["epoch"], r["retired"]["seq"]
    );
    Ok(())
}

fn migrate(project: &str, dry_run: bool, revert: bool) -> anyhow::Result<()> {
    let home = clawft_types::runtime_paths::home_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine the home directory"))?;
    let mdir = crate::user_daemon::manifests_dir(&home);
    let explain = |e: MigrateError| anyhow::anyhow!("{e}");
    if revert {
        let m = project_migrate::revert(&home, &mdir, project, dry_run).map_err(explain)?;
        println!(
            "{}project {} ({}): [serve] via = \"user-daemon\"",
            if dry_run { "(dry run) would set " } else { "set " },
            m.id,
            m.name
        );
        return Ok(());
    }
    let plan = project_migrate::plan(&mdir, project).map_err(explain)?;
    let m = &plan.manifest;
    println!("project {} ({}) at {}", m.id, m.name, m.root.display());
    for a in &plan.actions {
        match a {
            Action::Copy { from, to } => println!("  copy {} -> {}", from.display(), to.display()),
            Action::KeepExisting { to } => {
                println!("  keep {} (exists and differs; not overwritten)", to.display());
            }
            Action::SetVia(v) => println!("  set [serve] via = {v:?} in the manifest"),
        }
    }
    if dry_run {
        println!("(dry run: nothing changed)");
        return Ok(());
    }
    project_migrate::apply(&mdir, &plan).map_err(explain)?;
    println!("done. start it with `weaver kernel start --project {}`", m.id);
    println!("{}", project_migrate::rollback_line(m));
    Ok(())
}
