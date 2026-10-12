//! Project initialization for WeftOS.
//!
//! `init_project` sets up `.weftos/` and `weave.toml`. The host renderer
//! (`weftos init --claude|--grok|--codex`) lives in the submodules: it renders
//! the WeftOS agent packages into each host's layout ([`plan::plan`], then
//! [`apply::apply`]).

pub mod apply;
pub mod digest;
pub mod lock;
pub mod package;
pub mod plan;
pub mod render;
mod render_claude;
mod render_codex;
mod render_grok;
pub mod source;

pub use plan::{AgentInitOptions, Plan};
pub use render::Host;

use std::fs;
use std::path::{Path, PathBuf};

/// Result of initializing WeftOS in a project.
pub struct InitResult {
    pub project_root: PathBuf,
    pub weave_toml_created: bool,
    pub weftos_dir_created: bool,
    pub agents_installed: usize,
    pub skills_installed: usize,
}

/// Initialize WeftOS in a project directory.
pub fn init_project(project_root: impl AsRef<Path>) -> Result<InitResult, InitError> {
    let root = project_root.as_ref();
    let mut result = InitResult {
        project_root: root.to_path_buf(),
        weave_toml_created: false,
        weftos_dir_created: false,
        agents_installed: 0,
        skills_installed: 0,
    };

    // 1. Create .weftos/ directory for runtime state
    let weftos_dir = root.join(".weftos");
    if !weftos_dir.exists() {
        fs::create_dir_all(&weftos_dir)?;
        fs::create_dir_all(weftos_dir.join("chain"))?;
        fs::create_dir_all(weftos_dir.join("tree"))?;
        fs::create_dir_all(weftos_dir.join("logs"))?;
        fs::create_dir_all(weftos_dir.join("artifacts"))?;
        result.weftos_dir_created = true;
    }

    // 2. Generate weave.toml if not present
    let weave_toml = root.join("weave.toml");
    if !weave_toml.exists() {
        let config = generate_default_config(root);
        fs::write(&weave_toml, config)?;
        result.weave_toml_created = true;
    }

    // 3. Add .weftos/ to .gitignore if git project
    let gitignore = root.join(".gitignore");
    if gitignore.exists() {
        let content = fs::read_to_string(&gitignore).unwrap_or_default();
        if !content.contains(".weftos") {
            let mut f = fs::OpenOptions::new().append(true).open(&gitignore)?;
            use std::io::Write;
            writeln!(f, "\n# WeftOS runtime state")?;
            writeln!(f, ".weftos/")?;
        }
    }

    Ok(result)
}

fn generate_default_config(project_root: &Path) -> String {
    let project_name = project_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");

    // Detect project type
    let is_rust = project_root.join("Cargo.toml").exists();
    let is_node = project_root.join("package.json").exists();
    let is_python =
        project_root.join("pyproject.toml").exists() || project_root.join("setup.py").exists();
    let has_git = project_root.join(".git").exists();

    let language = if is_rust {
        "rust"
    } else if is_node {
        "javascript"
    } else if is_python {
        "python"
    } else {
        "generic"
    };

    let ext = match language {
        "rust" => "rs",
        "javascript" => "ts,js",
        "python" => "py",
        _ => "*",
    };

    let git_source = if has_git {
        "[sources.git]\npath = \".\"\nbranch = \"main\"\n"
    } else {
        "# No git repository detected\n"
    };

    format!(
        r#"# WeftOS Configuration
# Generated for: {project_name}

[domain]
name = "{project_name}"
language = "{language}"
description = "WeftOS-managed project"

[kernel]
max_processes = 64
health_check_interval_secs = 30

[tick]
interval_ms = 50
budget_ratio = 0.3
adaptive = true

[sources]
{git_source}
[sources.files]
root = "."
patterns = ["**/*.{ext}"]

[embedding]
provider = "mock-sha256"
dimensions = 384
batch_size = 16

[governance]
default_environment = "development"
risk_threshold = 0.9

[mesh]
enabled = false
bind_address = "127.0.0.1:9489"  # set 0.0.0.0:9489 (or an interface) for peers on other machines
seed_peers = []
"#
    )
}

#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum InitError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("project already initialized")]
    AlreadyInitialized,
    #[error("init error: {0}")]
    Other(String),
}
