//! `weft` -- CLI binary for the clawft AI assistant framework.
//!
//! Provides the following subcommands:
//!
//! - `weft agent` -- Start an interactive agent session or send a single message.
//! - `weft gateway` -- Start channels + agent loop (Telegram, Slack, etc.).
//! - `weft mcp` -- Manage MCP server connections (add / list / remove).
//! - `weft mcp-server` -- Run as an MCP tool server (stdio or --listen HTTP/SSE).
//! - `weft status` -- Show configuration status and diagnostics.
//! - `weft channels` -- Inspect channel configuration status.
//! - `weft cron` -- Manage scheduled (cron) jobs.

use clap::{CommandFactory, Parser, Subcommand};

mod commands;
mod completions;
mod help_text;
pub mod interactive;
mod markdown;
mod mcp_tools;

/// Full version string (package version + git stamp), baked by `build.rs`.
///
/// Format: `0.6.20 (14145f16-dirty 2026-07-05T17:20Z)`. Surfaced via
/// `weft --version` so an installed binary reports exactly which tree
/// produced it — the load-bearing signal for the daemon-mismatch guard.
const BUILD_VERSION: &str = env!("BUILD_VERSION");

/// Product-branded AI assistant CLI (WEFT-176). Binary name stays `weft`.
#[derive(Parser)]
#[command(
    name = "weft",
    about = "WeftOS AI assistant CLI",
    version = BUILD_VERSION,
    disable_help_subcommand = true
)]
struct Cli {
    /// Enable verbose (debug-level) logging.
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Use the kernel serving this project (ULID; see `weft project list`).
    #[arg(long, global = true, value_name = "ULID")]
    project: Option<String>,

    /// Use the kernel whose runtime directory is DIR (overrides WEFTOS_RUNTIME_DIR).
    #[arg(long, global = true, value_name = "DIR")]
    runtime: Option<std::path::PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

/// Top-level subcommands.
#[derive(Subcommand)]
enum Commands {
    /// Start an interactive agent session or send a single message.
    Agent(commands::agent::AgentArgs),

    /// Start the gateway (channels + agent loop).
    Gateway(commands::gateway::GatewayArgs),

    /// Chief-of-staff agent host: conversation API and push-to-talk (ADR-118).
    Cos(commands::cos_cmd::CosArgs),

    /// Manage MCP server connections (add / list / remove).
    Mcp(commands::mcp_cmd::McpArgs),

    /// Run as an MCP tool server over stdio.
    #[cfg(feature = "services")]
    McpServer(commands::mcp_server::McpServerArgs),

    /// Show configuration status.
    Status(commands::status::StatusArgs),

    /// Inspect channel configuration.
    Channels {
        #[command(subcommand)]
        action: ChannelsAction,
    },

    /// Manage scheduled (cron) jobs.
    Cron {
        #[command(subcommand)]
        action: CronAction,
    },

    /// Manage agent sessions.
    Sessions {
        #[command(subcommand)]
        action: SessionsCmd,
    },

    /// Read and search agent memory.
    Memory {
        #[command(subcommand)]
        action: MemoryCmd,
    },

    /// Show resolved configuration.
    Config {
        #[command(subcommand)]
        action: ConfigCmd,
    },

    /// Manage skills (list, show, install).
    Skills(commands::skills_cmd::SkillsArgs),

    /// Manage tools (list, show, search, deny/allow).
    Tools(commands::tools_cmd::ToolsArgs),

    /// Manage agents (list, show, use).
    Agents(commands::agents_cmd::AgentsArgs),

    /// Multi-agent swarm demo (AgentBus + SwarmCoordinator).
    Swarm(commands::swarm_cmd::SwarmArgs),

    /// Inspect task-delegation routing decisions.
    Delegate(commands::delegate_cmd::DelegateArgs),

    /// Context-router decision trace and replay (WEFT-336).
    Routing(commands::routing_cmd::RoutingArgs),

    /// Environment and multi-agent readiness checks.
    Doctor(commands::doctor::DoctorArgs),

    /// Manage projects (init, fork, list, show, seed).
    Project(commands::project_cmd::ProjectArgs),

    /// Manage workspaces.
    Workspace(commands::workspace_cmd::WorkspaceArgs),

    /// Issue, revoke and list daemon bearer tokens (ADR-102).
    Token(commands::token_cmd::TokenArgs),

    /// Initialize clawft config and workspace.
    Onboard(commands::onboard::OnboardArgs),

    /// Code analysis: extract graphs, detect topology, enrich docs.
    Analyze(commands::analyze_cmd::AnalyzeArgs),

    /// Run SOP assessment workflow (analyze codebase, report findings).
    Assess(commands::assess_cmd::AssessArgs),

    /// Plugin development tools (create, validate, package).
    Plugins(commands::plugins_cmd::PluginsArgs),

    /// Security scanning, auditing, and hardening.
    Security(commands::security_cmd::SecurityArgs),

    /// Start the web dashboard (gateway + API + browser).
    #[cfg(feature = "api")]
    Ui(commands::ui_cmd::UiArgs),

    /// Voice pipeline commands (setup, test, talk mode).
    #[cfg(feature = "voice")]
    Voice(commands::voice::VoiceArgs),

    /// Show help for a topic (skills, agents, tools, commands, config).
    Help(commands::help_cmd::HelpArgs),

    /// Update weft and weaver binaries to latest release.
    Update,

    /// Generate shell completions.
    Completions {
        /// Shell to generate for (bash, zsh, fish, powershell).
        shell: String,
    },
}

/// Subcommands for `weft sessions`.
#[derive(Subcommand)]
enum SessionsCmd {
    /// List all sessions.
    List {
        /// Config file path (overrides auto-discovery).
        #[arg(short, long)]
        config: Option<String>,
    },

    /// Inspect a specific session.
    Inspect {
        /// Session key to inspect.
        session_id: String,

        /// Config file path (overrides auto-discovery).
        #[arg(short, long)]
        config: Option<String>,
    },

    /// Delete a specific session.
    Delete {
        /// Session key to delete.
        session_id: String,

        /// Config file path (overrides auto-discovery).
        #[arg(short, long)]
        config: Option<String>,
    },

    /// Garbage-collect legacy underscore-encoded session files after migration.
    Gc {
        /// Report what would be removed without deleting.
        #[arg(long)]
        dry_run: bool,

        /// Config file path (overrides auto-discovery).
        #[arg(short, long)]
        config: Option<String>,
    },
}

/// Subcommands for `weft memory`.
#[derive(Subcommand)]
enum MemoryCmd {
    /// Display long-term memory (MEMORY.md).
    Show {
        /// Config file path (overrides auto-discovery).
        #[arg(short, long)]
        config: Option<String>,
    },

    /// Display session history (HISTORY.md).
    History {
        /// Config file path (overrides auto-discovery).
        #[arg(short, long)]
        config: Option<String>,
    },

    /// Search memory and history.
    Search {
        /// Search query.
        query: String,

        /// Maximum number of results.
        #[arg(long, default_value = "10")]
        limit: usize,

        /// Config file path (overrides auto-discovery).
        #[arg(short, long)]
        config: Option<String>,
    },

    /// Rebuild the vector memory index from MEMORY.md (WEFT-84).
    ///
    /// Always reindexes, even when the existing `memory.rvf` is newer than
    /// MEMORY.md. Bootstrap also rebuilds automatically when MEMORY.md is
    /// newer (mtime check).
    Reindex {
        /// Config file path (overrides auto-discovery).
        #[arg(short, long)]
        config: Option<String>,
    },

    /// Export memory to a file.
    Export {
        /// Agent ID to export memory for.
        #[arg(long)]
        agent: String,

        /// Output file path.
        #[arg(short, long)]
        output: String,

        /// Export format: "json" or "rvf" (default: "json").
        #[arg(long, default_value = "json")]
        format: String,

        /// Config file path (overrides auto-discovery).
        #[arg(short, long)]
        config: Option<String>,
    },

    /// Import memory from a file.
    Import {
        /// Agent ID to import memory into.
        #[arg(long)]
        agent: String,

        /// Input file path.
        #[arg(short, long)]
        input: String,

        /// Skip WITNESS chain validation.
        #[arg(long)]
        skip_verify: bool,

        /// Config file path (overrides auto-discovery).
        #[arg(short, long)]
        config: Option<String>,
    },
}

/// Subcommands for `weft config`.
#[derive(Subcommand)]
enum ConfigCmd {
    /// Show the full resolved configuration.
    Show {
        /// Config file path (overrides auto-discovery).
        #[arg(short, long)]
        config: Option<String>,
    },

    /// Show a specific configuration section.
    Section {
        /// Section name (e.g., "agents", "gateway", "channels").
        name: String,

        /// Config file path (overrides auto-discovery).
        #[arg(short, long)]
        config: Option<String>,
    },
}

/// Subcommands for `weft channels`.
#[derive(Subcommand)]
enum ChannelsAction {
    /// Show channel status table.
    Status {
        /// Config file path (overrides auto-discovery).
        #[arg(short, long)]
        config: Option<String>,
    },
}

/// Subcommands for `weft cron`.
#[derive(Subcommand)]
enum CronAction {
    /// List all cron jobs.
    List {
        /// Config file path (overrides auto-discovery).
        #[arg(short, long)]
        config: Option<String>,
    },

    /// Add a new cron job.
    Add {
        /// Human-readable job name.
        #[arg(long)]
        name: String,

        /// Cron expression (e.g. "0 9 * * Mon-Fri").
        #[arg(long)]
        schedule: String,

        /// Agent prompt to execute when the job fires.
        #[arg(long)]
        prompt: String,

        /// Config file path (overrides auto-discovery).
        #[arg(short, long)]
        config: Option<String>,
    },

    /// Remove a cron job by ID.
    Remove {
        /// Job ID to remove.
        job_id: String,

        /// Config file path (overrides auto-discovery).
        #[arg(short, long)]
        config: Option<String>,
    },

    /// Enable a cron job.
    Enable {
        /// Job ID to enable.
        job_id: String,

        /// Config file path (overrides auto-discovery).
        #[arg(short, long)]
        config: Option<String>,
    },

    /// Disable a cron job.
    Disable {
        /// Job ID to disable.
        job_id: String,

        /// Config file path (overrides auto-discovery).
        #[arg(short, long)]
        config: Option<String>,
    },

    /// Manually trigger a cron job.
    Run {
        /// Job ID to run.
        job_id: String,

        /// Config file path (overrides auto-discovery).
        #[arg(short, long)]
        config: Option<String>,
    },
}

fn main() -> anyhow::Result<()> {
    let mut cli = Cli::parse();
    if let Some(dir) = cli.runtime.take() {
        // `--runtime` also governs the code paths that read the environment
        // (`socket_path()`, `is_daemon_running()`, the kernel's node-key dir),
        // so export it before any of them can run. The tokio runtime has not
        // started yet, so this process is still single-threaded.
        let dir = std::path::absolute(&dir).unwrap_or(dir);
        // SAFETY: no other thread exists yet.
        unsafe { std::env::set_var(clawft_types::runtime_paths::RUNTIME_DIR_ENV, &dir) };
        cli.runtime = Some(dir);
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(cli))
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    if let Some(p) = &cli.project {
        clawft_types::project::validate_id(p).map_err(|_| {
            anyhow::anyhow!(
                "invalid --project {p:?}: expected a 26-character ULID (see `weft project list`)"
            )
        })?;
    }
    let project = cli.project.clone();
    commands::daemon_conn::set_flags(clawft_rpc::resolve::ResolveFlags {
        runtime: cli.runtime.clone(),
        project: project.clone(),
    });
    clawft_rpc::set_context(clawft_rpc::ClientContext {
        project,
        ..Default::default()
    });

    let default_filter = if cli.verbose { "debug" } else { "warn" };
    // Logs go to stderr: stdout carries `weft agent -m` replies and the
    // `weft mcp-server` stdio JSON-RPC stream.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| default_filter.into()),
        )
        .with_writer(std::io::stderr)
        .init();

    // Check for updates (non-blocking, cached 24h)
    clawft_rpc::version_check::check_for_updates();

    match cli.command {
        Commands::Agent(args) => commands::agent::run(args).await?,
        Commands::Gateway(args) => commands::gateway::run(args).await?,
        Commands::Cos(args) => commands::cos_cmd::run(args).await?,
        Commands::Mcp(args) => commands::mcp_cmd::run(args).await?,
        #[cfg(feature = "services")]
        Commands::McpServer(args) => commands::mcp_server::run(args).await?,
        Commands::Status(args) => commands::status::run(args).await?,
        Commands::Channels { action } => {
            let platform = clawft_platform::NativePlatform::new();
            match action {
                ChannelsAction::Status { config } => {
                    let cfg = commands::load_config(&platform, config.as_deref()).await?;
                    commands::channels::channels_status(&cfg);
                }
            }
        }
        Commands::Cron { action } => {
            let platform = clawft_platform::NativePlatform::new();
            match action {
                CronAction::List { config } => {
                    let cfg = commands::load_config(&platform, config.as_deref()).await?;
                    commands::cron::cron_list(&cfg).await?;
                }
                CronAction::Add {
                    name,
                    schedule,
                    prompt,
                    config,
                } => {
                    let cfg = commands::load_config(&platform, config.as_deref()).await?;
                    commands::cron::cron_add(name, schedule, prompt, &cfg).await?;
                }
                CronAction::Remove { job_id, config } => {
                    let cfg = commands::load_config(&platform, config.as_deref()).await?;
                    commands::cron::cron_remove(job_id, &cfg).await?;
                }
                CronAction::Enable { job_id, config } => {
                    let cfg = commands::load_config(&platform, config.as_deref()).await?;
                    commands::cron::cron_enable(job_id, true, &cfg).await?;
                }
                CronAction::Disable { job_id, config } => {
                    let cfg = commands::load_config(&platform, config.as_deref()).await?;
                    commands::cron::cron_enable(job_id, false, &cfg).await?;
                }
                CronAction::Run { job_id, config } => {
                    let cfg = commands::load_config(&platform, config.as_deref()).await?;
                    commands::cron::cron_run(job_id, &cfg).await?;
                }
            }
        }
        Commands::Sessions { action } => {
            let platform = clawft_platform::NativePlatform::new();
            match action {
                SessionsCmd::List { config } => {
                    let cfg = commands::load_config(&platform, config.as_deref()).await?;
                    commands::sessions::sessions_list(&cfg).await?;
                }
                SessionsCmd::Inspect { session_id, config } => {
                    let cfg = commands::load_config(&platform, config.as_deref()).await?;
                    commands::sessions::sessions_inspect(session_id, &cfg).await?;
                }
                SessionsCmd::Delete { session_id, config } => {
                    let cfg = commands::load_config(&platform, config.as_deref()).await?;
                    commands::sessions::sessions_delete(session_id, &cfg).await?;
                }
                SessionsCmd::Gc { dry_run, config } => {
                    let cfg = commands::load_config(&platform, config.as_deref()).await?;
                    commands::sessions::sessions_gc(dry_run, &cfg).await?;
                }
            }
        }
        Commands::Memory { action } => {
            let platform = clawft_platform::NativePlatform::new();
            match action {
                MemoryCmd::Show { config } => {
                    let cfg = commands::load_config(&platform, config.as_deref()).await?;
                    commands::memory_cmd::memory_show(&cfg).await?;
                }
                MemoryCmd::History { config } => {
                    let cfg = commands::load_config(&platform, config.as_deref()).await?;
                    commands::memory_cmd::memory_history(&cfg).await?;
                }
                MemoryCmd::Search {
                    query,
                    limit,
                    config,
                } => {
                    let cfg = commands::load_config(&platform, config.as_deref()).await?;
                    commands::memory_cmd::memory_search(&query, limit, &cfg).await?;
                }
                MemoryCmd::Reindex { config } => {
                    let cfg = commands::load_config(&platform, config.as_deref()).await?;
                    commands::memory_cmd::memory_reindex(&cfg).await?;
                }
                MemoryCmd::Export {
                    agent,
                    output,
                    format,
                    config,
                } => {
                    let cfg = commands::load_config(&platform, config.as_deref()).await?;
                    commands::memory_cmd::memory_export(&agent, &output, &format, &cfg).await?;
                }
                MemoryCmd::Import {
                    agent,
                    input,
                    skip_verify,
                    config,
                } => {
                    let cfg = commands::load_config(&platform, config.as_deref()).await?;
                    commands::memory_cmd::memory_import(&agent, &input, skip_verify, &cfg).await?;
                }
            }
        }
        Commands::Config { action } => {
            let platform = clawft_platform::NativePlatform::new();
            match action {
                ConfigCmd::Show { config } => {
                    let cfg = commands::load_config(&platform, config.as_deref()).await?;
                    commands::config_cmd::config_show(&cfg);
                }
                ConfigCmd::Section { name, config } => {
                    let cfg = commands::load_config(&platform, config.as_deref()).await?;
                    commands::config_cmd::config_section(&cfg, &name);
                }
            }
        }
        Commands::Skills(args) => commands::skills_cmd::run(args).await?,
        Commands::Tools(args) => commands::tools_cmd::run(args).await?,
        Commands::Agents(args) => commands::agents_cmd::run(args).await?,
        Commands::Swarm(args) => commands::swarm_cmd::run(args).await?,
        Commands::Delegate(args) => commands::delegate_cmd::run(args).await?,
        Commands::Routing(args) => commands::routing_cmd::run(args).await?,
        Commands::Project(args) => commands::project_cmd::run(args).await?,
        Commands::Workspace(args) => commands::workspace_cmd::run(args).await?,
        Commands::Token(args) => {
            let platform = clawft_platform::NativePlatform::new();
            commands::token_cmd::run(args, &platform).await?;
        }
        Commands::Onboard(args) => commands::onboard::run(args).await?,
        Commands::Analyze(args) => commands::analyze_cmd::run(args).await?,
        Commands::Assess(args) => commands::assess_cmd::run(args).await?,
        Commands::Plugins(args) => commands::plugins_cmd::run(args).await?,
        Commands::Security(args) => commands::security_cmd::run(args).await?,
        #[cfg(feature = "api")]
        Commands::Ui(args) => commands::ui_cmd::run(args).await?,
        #[cfg(feature = "voice")]
        Commands::Voice(args) => commands::voice::handle_voice(args).await?,
        Commands::Doctor(args) => commands::doctor::run(args).await?,
        Commands::Help(args) => commands::help_cmd::run(args)?,
        Commands::Update => {
            println!("weft v{BUILD_VERSION}");
            println!();
            // Check if weaver is available and delegate
            let weaver = std::process::Command::new("weaver")
                .args(["update", "install"])
                .status();
            match weaver {
                Ok(s) if s.success() => {}
                _ => {
                    println!("weaver not found — install manually:");
                    println!(
                        "  curl -fsSL https://github.com/WeftOS/weftos/releases/latest/download/clawft-cli-installer.sh | sh"
                    );
                }
            }
        }
        Commands::Completions { shell } => match completions::Shell::from_str(&shell) {
            Some(s) => {
                let mut cmd = Cli::command();
                completions::generate_completions(&s, &mut cmd);
            }
            None => {
                eprintln!("unsupported shell: {shell}");
                eprintln!("supported: {}", completions::Shell::all_names().join(", "));
                std::process::exit(1);
            }
        },
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_parses_without_error() {
        // Verify the clap derive macro produces a valid command structure.
        Cli::command().debug_assert();
    }

    #[test]
    fn project_and_global_flags_parse() {
        let cli = Cli::try_parse_from([
            "weft",
            "--project",
            "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "--runtime",
            "/r",
            "project",
            "init",
            "--fork",
            "--force",
            "--name",
            "x",
        ])
        .unwrap();
        assert_eq!(cli.project.as_deref(), Some("01ARZ3NDEKTSV4RRFFQ69G5FAV"));
        assert_eq!(cli.runtime.as_deref(), Some(std::path::Path::new("/r")));
        assert!(matches!(cli.command, Commands::Project(_)));
        // Global: also accepted after the subcommand.
        assert!(
            Cli::try_parse_from(["weft", "project", "list", "--runtime", "/r", "--json"]).is_ok()
        );
        // --force without --fork, and --here with a target, are rejected.
        assert!(Cli::try_parse_from(["weft", "project", "init", "--force"]).is_err());
        assert!(Cli::try_parse_from(["weft", "project", "show", "x", "--here"]).is_err());
    }

    #[test]
    fn cli_help_contains_binary_name() {
        let help = Cli::command().render_help().to_string();
        assert!(help.contains("weft"));
    }

    #[test]
    fn cli_has_all_subcommands() {
        let cmd = Cli::command();
        let sub_names: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        assert!(sub_names.contains(&"agent"));
        assert!(sub_names.contains(&"gateway"));
        assert!(sub_names.contains(&"mcp"));
        assert!(sub_names.contains(&"mcp-server"));
        assert!(sub_names.contains(&"status"));
        assert!(sub_names.contains(&"channels"));
        assert!(sub_names.contains(&"cron"));
        assert!(sub_names.contains(&"sessions"));
        assert!(sub_names.contains(&"memory"));
        assert!(sub_names.contains(&"config"));
        assert!(sub_names.contains(&"skills"));
        assert!(sub_names.contains(&"tools"));
        assert!(sub_names.contains(&"agents"));
        assert!(sub_names.contains(&"swarm"));
        assert!(sub_names.contains(&"delegate"));
        assert!(sub_names.contains(&"workspace"));
        assert!(sub_names.contains(&"assess"));
        assert!(sub_names.contains(&"plugins"));
        assert!(sub_names.contains(&"onboard"));
        assert!(sub_names.contains(&"ui"));
        // kernel commands moved to `weaver` binary (clawft-weave crate)
        assert!(!sub_names.contains(&"kernel"));
        assert!(sub_names.contains(&"help"));
        assert!(sub_names.contains(&"completions"));
    }

    // ── Skills subcommand parsing ──────────────────────────────────

    #[test]
    fn cli_skills_list_parses() {
        let result = Cli::try_parse_from(["weft", "skills", "list"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_skills_show_parses() {
        let result = Cli::try_parse_from(["weft", "skills", "show", "research"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_skills_install_parses() {
        let result = Cli::try_parse_from(["weft", "skills", "install", "/path/to/skill"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_skills_remove_parses() {
        let result = Cli::try_parse_from(["weft", "skills", "remove", "old-skill"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_skills_search_parses() {
        let result = Cli::try_parse_from(["weft", "skills", "search", "coding"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_skills_search_with_limit_parses() {
        let result = Cli::try_parse_from(["weft", "skills", "search", "coding", "--limit", "5"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_skills_publish_parses() {
        let result = Cli::try_parse_from(["weft", "skills", "publish", "/path/to/skill"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_skills_publish_unsigned_parses() {
        let result = Cli::try_parse_from([
            "weft",
            "skills",
            "publish",
            "/path/to/skill",
            "--allow-unsigned",
        ]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_skills_remote_install_parses() {
        let result = Cli::try_parse_from(["weft", "skills", "remote-install", "coding-agent"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_skills_remote_install_unsigned_parses() {
        let result = Cli::try_parse_from([
            "weft",
            "skills",
            "remote-install",
            "coding-agent",
            "--allow-unsigned",
        ]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_skills_keygen_parses() {
        let result = Cli::try_parse_from(["weft", "skills", "keygen"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_skills_autogen_enable_parses() {
        let result = Cli::try_parse_from(["weft", "skills", "autogen", "enable"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_skills_autogen_disable_parses() {
        let result = Cli::try_parse_from(["weft", "skills", "autogen", "disable"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_skills_autogen_status_parses() {
        let result = Cli::try_parse_from(["weft", "skills", "autogen", "status"]);
        assert!(result.is_ok());
    }

    // ── Tools subcommand parsing ───────────────────────────────────

    #[test]
    fn cli_skills_refresh_parses() {
        let result = Cli::try_parse_from(["weft", "skills", "refresh"]);
        assert!(result.is_ok());
        if let Commands::Skills(args) = result.unwrap().command {
            assert!(matches!(
                args.action,
                commands::skills_cmd::SkillsAction::Refresh
            ));
        } else {
            panic!("expected Skills command");
        }
    }

    #[test]
    fn cli_tools_list_parses() {
        let result = Cli::try_parse_from(["weft", "tools", "list"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_tools_show_parses() {
        let result = Cli::try_parse_from(["weft", "tools", "show", "read_file"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_tools_mcp_parses() {
        let result = Cli::try_parse_from(["weft", "tools", "mcp"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_tools_search_parses() {
        let result = Cli::try_parse_from(["weft", "tools", "search", "web"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_tools_deny_parses() {
        let result = Cli::try_parse_from(["weft", "tools", "deny", "exec_*"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_tools_allow_parses() {
        let result = Cli::try_parse_from(["weft", "tools", "allow", "exec_*"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_tools_list_with_config_parses() {
        let result = Cli::try_parse_from(["weft", "tools", "list", "--config", "/tmp/config.json"]);
        assert!(result.is_ok());
    }

    // ── Agents subcommand parsing ──────────────────────────────────

    #[test]
    fn cli_agents_list_parses() {
        let result = Cli::try_parse_from(["weft", "agents", "list"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_agents_show_parses() {
        let result = Cli::try_parse_from(["weft", "agents", "show", "researcher"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_agents_use_parses() {
        let result = Cli::try_parse_from(["weft", "agents", "use", "coder"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_verbose_flag_is_global() {
        // --verbose before subcommand should parse correctly.
        let result = Cli::try_parse_from(["weft", "--verbose", "status"]);
        assert!(result.is_ok());
        let cli = result.unwrap();
        assert!(cli.verbose);
    }

    #[test]
    fn cli_agent_subcommand_parses_message() {
        let result = Cli::try_parse_from(["weft", "agent", "--message", "hello world"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_agent_subcommand_parses_model() {
        let result = Cli::try_parse_from(["weft", "agent", "--model", "openai/gpt-4"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_gateway_subcommand_parses_config() {
        let result = Cli::try_parse_from(["weft", "gateway", "--config", "/tmp/config.json"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_status_detailed_flag() {
        let result = Cli::try_parse_from(["weft", "status", "--detailed"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_channels_status_parses() {
        let result = Cli::try_parse_from(["weft", "channels", "status"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_channels_status_with_config() {
        let result =
            Cli::try_parse_from(["weft", "channels", "status", "--config", "/tmp/config.json"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_cron_list_parses() {
        let result = Cli::try_parse_from(["weft", "cron", "list"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_cron_add_parses() {
        let result = Cli::try_parse_from([
            "weft",
            "cron",
            "add",
            "--name",
            "daily report",
            "--schedule",
            "0 9 * * *",
            "--prompt",
            "Generate report",
        ]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_cron_remove_parses() {
        let result = Cli::try_parse_from(["weft", "cron", "remove", "job-123"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_cron_enable_parses() {
        let result = Cli::try_parse_from(["weft", "cron", "enable", "job-123"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_cron_disable_parses() {
        let result = Cli::try_parse_from(["weft", "cron", "disable", "job-123"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_cron_run_parses() {
        let result = Cli::try_parse_from(["weft", "cron", "run", "job-123"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_onboard_parses() {
        let result = Cli::try_parse_from(["weft", "onboard"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_onboard_yes_flag() {
        let result = Cli::try_parse_from(["weft", "onboard", "--yes"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_onboard_dir_override() {
        let result = Cli::try_parse_from(["weft", "onboard", "--dir", "/tmp/test-clawft"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_onboard_short_yes_flag() {
        let result = Cli::try_parse_from(["weft", "onboard", "-y"]);
        assert!(result.is_ok());
    }

    // ── MCP manage subcommand parsing (WEFT-188) ───────────────────

    #[test]
    fn cli_swarm_demo_parses() {
        let result = Cli::try_parse_from(["weft", "swarm", "demo", "--workers", "4"]);
        assert!(result.is_ok(), "parse error: {}", result.err().unwrap());
    }

    #[test]
    fn cli_mcp_list_parses() {
        let result = Cli::try_parse_from(["weft", "mcp", "list"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_mcp_add_command_parses() {
        let result = Cli::try_parse_from([
            "weft",
            "mcp",
            "add",
            "demo",
            "--command",
            "npx",
            "--arg",
            "-y",
            "--arg",
            "@example/mcp",
            "--env",
            "TOKEN=x",
        ]);
        assert!(result.is_ok(), "parse error: {}", result.err().unwrap());
    }

    #[test]
    fn cli_mcp_add_url_parses() {
        let result = Cli::try_parse_from([
            "weft",
            "mcp",
            "add",
            "remote",
            "--url",
            "https://mcp.example.com/sse",
        ]);
        assert!(result.is_ok(), "parse error: {}", result.err().unwrap());
    }

    #[test]
    fn cli_mcp_add_trailing_parses() {
        let result =
            Cli::try_parse_from(["weft", "mcp", "add", "trail", "--", "uvx", "my-mcp"]);
        assert!(result.is_ok(), "parse error: {}", result.err().unwrap());
    }

    #[test]
    fn cli_mcp_remove_parses() {
        let result = Cli::try_parse_from(["weft", "mcp", "remove", "demo"]);
        assert!(result.is_ok());
    }

    #[cfg(feature = "services")]
    #[test]
    fn cli_mcp_server_parses() {
        let result = Cli::try_parse_from(["weft", "mcp-server"]);
        assert!(result.is_ok());
        match result.unwrap().command {
            Commands::McpServer(args) => {
                assert_eq!(args.profile, "default");
                assert!(args.config.is_none());
                assert!(!args.reexport_mcp);
                assert!(!args.attach, "standalone is the default");
            }
            _ => panic!("expected McpServer"),
        }
    }

    #[cfg(feature = "services")]
    #[test]
    fn cli_mcp_server_with_config() {
        let result = Cli::try_parse_from(["weft", "mcp-server", "--config", "/tmp/config.json"]);
        assert!(result.is_ok());
    }

    #[cfg(feature = "services")]
    #[test]
    fn cli_mcp_server_with_profile() {
        let result = Cli::try_parse_from(["weft", "mcp-server", "--profile", "control,media"]);
        assert!(result.is_ok(), "parse error: {:?}", result.err());
        match result.unwrap().command {
            Commands::McpServer(args) => {
                assert_eq!(args.profile, "control,media");
                assert!(!args.reexport_mcp);
            }
            _ => panic!("expected McpServer"),
        }
    }

    #[cfg(feature = "services")]
    #[test]
    fn cli_mcp_server_profile_full() {
        let result = Cli::try_parse_from(["weft", "mcp-server", "--profile", "full"]);
        assert!(result.is_ok());
        match result.unwrap().command {
            Commands::McpServer(args) => {
                assert_eq!(args.profile, "full");
                assert!(!args.reexport_mcp, "reexport off unless flag");
            }
            _ => panic!("expected McpServer"),
        }
    }

    #[cfg(feature = "services")]
    #[test]
    fn cli_mcp_server_reexport_mcp_flag() {
        let result = Cli::try_parse_from([
            "weft",
            "mcp-server",
            "--profile",
            "full",
            "--reexport-mcp",
        ]);
        assert!(result.is_ok(), "parse error: {:?}", result.err());
        match result.unwrap().command {
            Commands::McpServer(args) => {
                assert_eq!(args.profile, "full");
                assert!(args.reexport_mcp);
            }
            _ => panic!("expected McpServer"),
        }
    }

    #[cfg(feature = "services")]
    #[test]
    fn cli_mcp_server_attach_parses() {
        let result = Cli::try_parse_from(["weft", "mcp-server", "--attach"]);
        assert!(result.is_ok());
        match result.unwrap().command {
            Commands::McpServer(args) => {
                assert!(args.attach);
                assert!(!args.reexport_mcp);
            }
            _ => panic!("expected McpServer"),
        }
    }

    #[test]
    fn cli_mcp_server_attach_with_profile() {
        let result =
            Cli::try_parse_from(["weft", "mcp-server", "--attach", "--profile", "control"]);
        assert!(result.is_ok());
        match result.unwrap().command {
            Commands::McpServer(args) => {
                assert!(args.attach);
                assert_eq!(args.profile, "control");
            }
            _ => panic!("expected McpServer"),
        }
    }

    #[test]
    fn cli_mcp_server_verbose() {
        let result = Cli::try_parse_from(["weft", "--verbose", "mcp-server"]);
        assert!(result.is_ok());
        let cli = result.unwrap();
        assert!(cli.verbose);
    }

    #[cfg(feature = "services")]
    #[test]
    fn cli_mcp_server_listen_parses() {
        let result = Cli::try_parse_from([
            "weft",
            "mcp-server",
            "--listen",
            "127.0.0.1:8742",
            "--token-env",
            "WEFT_MCP_TOKEN",
            "--enterprise",
            "--dangerously-bind-public",
        ]);
        assert!(result.is_ok(), "parse error: {:?}", result.err());
        match result.unwrap().command {
            Commands::McpServer(args) => {
                assert_eq!(args.listen.as_deref(), Some("127.0.0.1:8742"));
                assert_eq!(args.token_env.as_deref(), Some("WEFT_MCP_TOKEN"));
                assert!(args.enterprise);
                assert!(args.dangerously_bind_public);
                assert!(!args.allow_unauthenticated);
            }
            _ => panic!("expected McpServer"),
        }
    }

    // ── Workspace subcommand parsing ────────────────────────────────

    #[test]
    fn cli_workspace_create_parses() {
        let result = Cli::try_parse_from(["weft", "workspace", "create", "my-ws"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_workspace_create_with_dir() {
        let result = Cli::try_parse_from([
            "weft",
            "workspace",
            "create",
            "my-ws",
            "--dir",
            "/tmp/projects",
        ]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_workspace_list_parses() {
        let result = Cli::try_parse_from(["weft", "workspace", "list"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_workspace_list_all() {
        let result = Cli::try_parse_from(["weft", "workspace", "list", "--all"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_workspace_load_parses() {
        let result = Cli::try_parse_from(["weft", "workspace", "load", "my-ws"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_workspace_status_parses() {
        let result = Cli::try_parse_from(["weft", "workspace", "status"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_workspace_delete_parses() {
        let result = Cli::try_parse_from(["weft", "workspace", "delete", "my-ws"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_workspace_delete_yes() {
        let result = Cli::try_parse_from(["weft", "workspace", "delete", "my-ws", "-y"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_workspace_config_set_parses() {
        let result = Cli::try_parse_from([
            "weft",
            "workspace",
            "config",
            "set",
            "agents.defaults.model",
            "openai/gpt-4o",
        ]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_workspace_config_get_parses() {
        let result = Cli::try_parse_from([
            "weft",
            "workspace",
            "config",
            "get",
            "agents.defaults.model",
        ]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_workspace_config_reset_parses() {
        let result = Cli::try_parse_from(["weft", "workspace", "config", "reset"]);
        assert!(result.is_ok());
    }

    // ── Help subcommand parsing ───────────────────────────────────

    #[test]
    fn cli_help_no_topic_parses() {
        let result = Cli::try_parse_from(["weft", "help"]);
        assert!(result.is_ok());
    }

    #[test]
    fn cli_help_with_topic_parses() {
        let result = Cli::try_parse_from(["weft", "help", "skills"]);
        assert!(result.is_ok());
    }
}
