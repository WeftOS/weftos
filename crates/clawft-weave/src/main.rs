//! `weaver` — WeftOS operator CLI.
//!
//! The human-facing CLI for kernel management, agent orchestration,
//! and system administration. Complement to `weft` (the agent CLI).
//!
//! # Commands
//!
//! - `weaver kernel` — Boot, status, process table, services.
//! - `weaver agent` — Spawn, stop, restart, inspect agents (planned).
//! - `weaver app` — Install, start, stop applications (planned).
//! - `weaver ipc` — Send messages, manage topics (planned).

use clap::{Parser, Subcommand};

use clawft_weave::commands;

/// Full version string (package version + git stamp), baked by `build.rs`.
///
/// Format: `0.6.20 (14145f16-dirty 2026-07-05T17:20Z)`. Surfaced via
/// `weaver --version`; the daemon reports the same stamp over
/// `kernel.status` so a stale binary/daemon pairing is caught.
const BUILD_VERSION: &str = env!("BUILD_VERSION");

/// WeftOS operator CLI.
#[derive(Parser)]
#[command(
    name = "weaver",
    about = "WeftOS operator CLI — kernel, agents, and system management",
    version = BUILD_VERSION,
    disable_help_subcommand = true
)]
struct Cli {
    /// Enable verbose (debug-level) logging.
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Commands,
}

/// Top-level subcommands.
#[derive(Subcommand)]
enum Commands {
    /// Kernel management (boot, status, services, processes).
    Kernel(commands::kernel_cmd::KernelArgs),

    /// Per-project kernel operations (migrate-kernel).
    #[cfg(all(unix, feature = "exochain", feature = "placement"))]
    Project(commands::project_cmd::ProjectArgs),

    /// Agent lifecycle management (spawn, stop, restart, inspect).
    Agent(commands::agent_cmd::AgentArgs),

    /// Application management (install, start, stop, list).
    App(commands::app_cmd::AppArgs),

    /// Governed workloads placed across the mesh (ADR-099).
    Workload(commands::workload_cmd::WorkloadArgs),

    /// Adopt, verify and explain locally held model weights (ADR-101).
    #[cfg(all(feature = "ecc", feature = "exochain"))]
    Model(commands::model_cmd::ModelArgs),

    /// Project cog sources (WeftOS, Cognitum, private), licences, search, install (ADR-105).
    Cog(commands::cog_cmd::CogArgs),

    /// Machine mesh service: serve it, inspect it, administer bindings and peers (ADR-103).
    #[cfg(all(unix, feature = "mesh"))]
    Mesh(commands::mesh_cmd::MeshArgs),

    /// Cluster management (nodes, shards, health).
    Cluster(commands::cluster_cmd::ClusterArgs),

    /// Fleet manager: every known node with trust, last seen, cogs and location.
    Fleet(commands::fleet_cmd::FleetArgs),

    /// Dashboard reporter: status and token rotation (locally or on a peer).
    Dashboard(commands::dashboard_cmd::DashboardArgs),

    /// What this node exposes to the mesh: list, on, off, pause, log (ADR-120).
    Expose(commands::expose_cmd::ExposeArgs),

    /// The node's cog data plane broker: list topics (ADR-122).
    Cogbus(commands::cogbus_cmd::CogbusArgs),

    /// Tailnet router: routes, reload, and the Tailscale Serve front door (ADR-116).
    Route(commands::route_cmd::RouteArgs),

    /// Chain management (status, events, checkpoints).
    Chain(commands::chain_cmd::ChainArgs),

    /// Custody attestation (signed proof of system state).
    Custody(commands::custody_cmd::CustodyArgs),

    /// Resource tree management (tree, inspect, stats).
    Resource(commands::resource_cmd::ResourceArgs),

    /// Identity-journal operator commands (promote, status).
    Soul(commands::soul_cmd::SoulArgs),

    /// Cron job management (add, list, remove).
    Cron(commands::cron_cmd::CronArgs),

    /// IPC management (topics, subscribe, publish).
    Ipc(commands::ipc_cmd::IpcArgs),

    /// Interactive kernel console (boot + REPL, or attach to running kernel).
    #[cfg(unix)]
    Console(commands::console_cmd::ConsoleArgs),

    /// ECC cognitive substrate management (status, calibrate, search).
    Ecc(commands::ecc_cmd::EccArgs),

    /// Knowledge graph extraction, query, and export (graphify).
    Graphify(commands::graphify_cmd::GraphifyArgs),

    /// Version index of first-class objects: list, tree, deps (ADR-121).
    Versions(commands::versions_cmd::VersionsArgs),

    /// Obsidian vault cultivation (frontmatter, links, graph analysis).
    Vault(commands::vault_cmd::VaultArgs),

    /// Topology layout, schema validation, and geometry detection.
    Topology(commands::topology_cmd::TopologyArgs),

    /// Leaf device control (push audio, display, effects).
    Leaf(commands::leaf_cmd::LeafArgs),

    /// Run standardized kernel performance benchmark.
    Benchmark {
        #[command(subcommand)]
        cmd: commands::bench_cmd::BenchCmd,
    },

    /// Print a launchd / systemd unit for the per-user daemon.
    Service(commands::service_cmd::ServiceArgs),

    /// Initialize development environment (install skills, verify tools).
    Init(commands::init_cmd::InitArgs),

    /// Update weft, weaver and weftos to the latest verified release.
    Update(commands::update_cmd::UpdateArgs),

    /// Check install, daemon, runtime and MCP health (read-only; `--fix` for stale files).
    Doctor(commands::doctor_cmd::DoctorArgs),

    /// Migrate WeftOS state (user chain).
    Migrate(commands::migrate_cmd::MigrateArgs),

    /// Show version and build info.
    Version,
}

fn main() -> anyhow::Result<()> {
    #[cfg(all(unix, feature = "exochain", feature = "placement"))]
    {
        let args: Vec<String> = std::env::args().skip(1).collect();
        if let Err(error) = clawft_weave::project_supervisor::sandbox::run_helper(&args) {
            anyhow::bail!("{error}");
        }
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = rt.block_on(async_main());
    // Dropping the runtime waits for every blocking task without limit; a
    // wedged one would keep an exiting daemon alive (card 1a919ae6).
    rt.shutdown_timeout(std::time::Duration::from_secs(2));
    result
}

async fn async_main() -> anyhow::Result<()> {
    // Best-effort load of `.env` from the current working directory,
    // before any subcommand reads env vars. Lets `OPENROUTER_API_KEY`,
    // `LLM_SERVICE_URL`, `LLM_MODEL`, etc. live in a project-local
    // `.env` (which is gitignored) without forcing shell exports.
    // Silently ignored if no file exists.
    let _ = dotenvy::dotenv();

    let cli = Cli::parse();

    // WEFT-597 / BUG-3: install the chain_event → pending-buffer Layer
    // before any subcommand runs. The daemon drain loop (exochain feature)
    // periodically appends drained records to ChainManager. Without this
    // Layer, tracing-only emitters (graphify, soul, project.init, …) hit
    // stdout and never reach ExoChain.
    let default_filter = if cli.verbose { "debug" } else { "warn" };
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| default_filter.into()),
        )
        .with(clawft_weave::chain_bridge::ChainEventLayer::new())
        // Logs go to stderr: stdout carries command output and JSON.
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();

    // Check for updates (non-blocking, cached 24h)
    clawft_rpc::version_check::check_for_updates();

    match cli.command {
        Commands::Kernel(args) => commands::kernel_cmd::run(args).await?,
        #[cfg(all(unix, feature = "exochain", feature = "placement"))]
        Commands::Project(args) => commands::project_cmd::run(args).await?,
        Commands::Agent(args) => commands::agent_cmd::run(args).await?,
        Commands::App(args) => commands::app_cmd::run(args).await?,
        Commands::Workload(args) => commands::workload_cmd::run(args).await?,
        #[cfg(all(feature = "ecc", feature = "exochain"))]
        Commands::Model(args) => commands::model_cmd::run(args)?,
        Commands::Cog(args) => commands::cog_cmd::run(args).await?,
        Commands::Cluster(args) => commands::cluster_cmd::run(args).await?,
        Commands::Fleet(args) => commands::fleet_cmd::run(args).await?,
        Commands::Dashboard(args) => commands::dashboard_cmd::run(args).await?,
        Commands::Expose(args) => commands::expose_cmd::run(args).await?,
        Commands::Cogbus(args) => commands::cogbus_cmd::run(args).await?,
        Commands::Route(args) => commands::route_cmd::run(args).await?,
        Commands::Chain(args) => commands::chain_cmd::run(args).await?,
        Commands::Custody(args) => commands::custody_cmd::run(args).await?,
        Commands::Resource(args) => commands::resource_cmd::run(args).await?,
        Commands::Soul(args) => commands::soul_cmd::run(args).await?,
        Commands::Cron(args) => commands::cron_cmd::run(args).await?,
        Commands::Ipc(args) => commands::ipc_cmd::run(args).await?,
        #[cfg(unix)]
        Commands::Console(args) => commands::console_cmd::run(args).await?,
        Commands::Ecc(args) => commands::ecc_cmd::run(args).await?,
        Commands::Graphify(args) => commands::graphify_cmd::run(args).await?,
        Commands::Vault(args) => commands::vault_cmd::run(args).await?,
        Commands::Versions(args) => commands::versions_cmd::run(args).await?,
        Commands::Topology(args) => commands::topology_cmd::run(args).await?,
        Commands::Leaf(args) => commands::leaf_cmd::run(args).await?,
        Commands::Benchmark { cmd } => commands::bench_cmd::run(cmd).await?,
        Commands::Update(args) => commands::update_cmd::run(args).await?,
        #[cfg(all(unix, feature = "mesh"))]
        Commands::Mesh(args) => commands::mesh_cmd::run(args).await?,
        Commands::Service(args) => commands::service_cmd::run(args).await?,
        Commands::Init(args) => commands::init_cmd::run(args).await?,
        Commands::Doctor(args) => commands::doctor_cmd::run(args).await?,
        Commands::Migrate(args) => commands::migrate_cmd::run(args)?,
        Commands::Version => {
            println!(
                "weaver {} (WeftOS) · git {} · built {}",
                env!("CARGO_PKG_VERSION"),
                env!("BUILD_GIT_HASH"),
                env!("BUILD_TIMESTAMP"),
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_parses_without_error() {
        Cli::command().debug_assert();
    }

    #[test]
    fn cli_help_contains_binary_name() {
        let help = Cli::command().render_help().to_string();
        assert!(help.contains("weaver"));
    }
}
