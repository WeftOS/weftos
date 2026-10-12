//! WeftOS daemon -- boots the kernel in any project directory.

use clap::Parser;

mod team_cmd;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "weftos", about = "WeftOS: AI kernel for any project")]
enum Cli {
    /// Initialize WeftOS in the current directory
    ///
    /// With --claude/--grok/--codex, renders the WeftOS agent packages into
    /// those hosts' layouts instead (plan by default; --apply writes, and
    /// never commits or pushes).
    Init {
        /// Project directory (default: current)
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Force reinitialize; with a host flag, overwrite drifted files
        #[arg(long)]
        force: bool,
        /// Render agents for Claude Code (.claude/agents, .claude/skills)
        #[arg(long)]
        claude: bool,
        /// Render agents for Grok Build (.grok/agents, .grok/skills)
        #[arg(long)]
        grok: bool,
        /// Render agents for Codex (.codex/agents, .agents/skills, AGENTS.md)
        #[arg(long)]
        codex: bool,
        /// Print the change plan without writing (default)
        #[arg(long, conflicts_with = "apply")]
        plan: bool,
        /// Write the rendered files and .weftos/agents.lock.json
        #[arg(long)]
        apply: bool,
        /// Render into ~/.claude, ~/.grok, ~/.codex instead of the project
        #[arg(long)]
        global: bool,
        /// Use an agents/ source tree on disk instead of the embedded one
        #[arg(long, value_name = "DIR")]
        from: Option<PathBuf>,
        /// Team to install (agents/teams/<team>/team.yaml; default weftos-core)
        #[arg(long)]
        team: Option<String>,
        /// Install an individual agent package (repeatable)
        #[arg(long = "agent", value_name = "ID")]
        agents: Vec<String>,
    },
    /// Apply, inspect and remove agent teams in a project
    Team {
        #[command(subcommand)]
        cmd: team_cmd::TeamCmd,
    },
    /// Boot the WeftOS kernel
    Boot {
        /// Project directory
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Show WeftOS status
    Status,
    /// Show version
    Version,
}

#[tokio::main]
async fn main() {
    // Logs go to stderr so stdout stays command output.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    match cli {
        Cli::Init {
            path,
            force,
            claude,
            grok,
            codex,
            plan,
            apply,
            global,
            from,
            team,
            agents,
        } => {
            let hosts: Vec<weftos::init::Host> = [
                (claude, weftos::init::Host::Claude),
                (grok, weftos::init::Host::Grok),
                (codex, weftos::init::Host::Codex),
            ]
            .into_iter()
            .filter_map(|(on, h)| on.then_some(h))
            .collect();
            let agent_flags =
                plan || apply || global || from.is_some() || team.is_some() || !agents.is_empty();
            if !hosts.is_empty() {
                let root = if global {
                    match std::env::var_os("HOME") {
                        Some(h) => PathBuf::from(h),
                        None => {
                            eprintln!("--global needs HOME set");
                            std::process::exit(1);
                        }
                    }
                } else {
                    path
                };
                let opts = weftos::init::AgentInitOptions {
                    root,
                    hosts,
                    apply,
                    force,
                    global,
                    from,
                    team,
                    agents,
                    ..Default::default()
                };
                std::process::exit(run_agent_init(&opts));
            }
            if agent_flags {
                eprintln!(
                    "--plan/--apply/--global/--from/--team/--agent need a host flag: --claude, --grok and/or --codex"
                );
                std::process::exit(2);
            }
            if weftos::is_initialized(&path) && !force {
                eprintln!(
                    "WeftOS already initialized in {}. Use --force to reinitialize.",
                    path.display()
                );
                std::process::exit(1);
            }
            match weftos::init::init_project(&path) {
                Ok(result) => {
                    println!("WeftOS initialized in {}", result.project_root.display());
                    if result.weave_toml_created {
                        println!("  Created weave.toml");
                    }
                    if result.weftos_dir_created {
                        println!("  Created .weftos/ directory");
                    }
                    println!("\nNext: weftos boot");
                }
                Err(e) => {
                    eprintln!("Init failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        Cli::Team { cmd } => std::process::exit(team_cmd::run(cmd)),
        Cli::Boot { path } => {
            if !weftos::is_initialized(&path) {
                eprintln!("WeftOS not initialized. Run: weftos init");
                std::process::exit(1);
            }
            println!("Booting WeftOS in {}...", path.display());
            match weftos::WeftOs::boot_in(&path).await {
                Ok(os) => {
                    println!("WeftOS running");
                    println!("  State: {:?}", os.state());
                    println!("  Services: {}", os.service_count());
                    println!("  Processes: {}", os.process_count());
                    println!("\nPress Ctrl+C to stop.");
                    tokio::signal::ctrl_c().await.ok();
                    println!("\nShutting down...");
                    if let Err(e) = os.shutdown().await {
                        eprintln!("Shutdown error: {e}");
                    }
                }
                Err(e) => {
                    eprintln!("Boot failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        Cli::Status => {
            if weftos::is_initialized(".") {
                println!("WeftOS initialized in current directory");
                if std::path::Path::new("weave.toml").exists() {
                    println!("  Config: weave.toml");
                }
                if std::path::Path::new(".weftos").exists() {
                    println!("  Runtime: .weftos/");
                }
            } else {
                println!("WeftOS not initialized. Run: weftos init");
            }
        }
        Cli::Version => {
            println!("weftos {}", weftos::VERSION);
        }
    }
}

/// `weftos init --claude|--grok|--codex`: plan, optionally apply, report.
fn run_agent_init(opts: &weftos::init::AgentInitOptions) -> i32 {
    use weftos::init::{apply, plan};
    let plan = match plan::plan(opts) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("weftos init: {e}");
            return 1;
        }
    };
    if opts.apply
        && let Err(e) = apply::apply(&plan)
    {
        eprintln!("weftos init: apply failed: {e}");
        return 1;
    }
    print!("{}", apply::format_plan(&plan, opts.apply));
    0
}
