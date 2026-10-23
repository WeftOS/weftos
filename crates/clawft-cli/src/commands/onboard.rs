//! `weft onboard` -- Initialize clawft configuration and workspace.
//!
//! Creates the `~/.clawft/` directory structure, generates a config template,
//! and optionally prompts for API key configuration.

use std::io::{self, BufRead, IsTerminal, Write as IoWrite};
use std::path::{Path, PathBuf};

use clap::Args;
use serde_json::json;

/// Arguments for `weft onboard`.
#[derive(Args)]
pub struct OnboardArgs {
    /// Skip interactive prompts and use defaults.
    #[arg(long, short = 'y')]
    pub yes: bool,

    /// Override the config directory (default: ~/.clawft).
    #[arg(long)]
    pub dir: Option<String>,
}

/// Subdirectories created under the config root.
const WORKSPACE_DIRS: &[&str] = &["sessions", "memory", "skills"];

/// Run the onboard wizard.
pub async fn run(args: OnboardArgs) -> anyhow::Result<()> {
    let config_dir = resolve_config_dir(args.dir.as_deref())?;
    let config_path = config_dir.join("config.json");

    println!("clawft onboarding");
    println!("=================");
    println!();

    // Check if already initialized
    if config_path.exists() {
        if args.yes {
            println!("Config already exists at {}", config_path.display());
            println!("Skipping (--yes mode).");
            return Ok(());
        }
        print!(
            "Config already exists at {}. Overwrite? [y/N] ",
            config_path.display()
        );
        io::stdout().flush()?;
        let mut line = String::new();
        io::stdin().lock().read_line(&mut line)?;
        if !line.trim().eq_ignore_ascii_case("y") {
            println!("Aborted.");
            return Ok(());
        }
    }

    // Create directory structure
    create_directory_structure(&config_dir)?;

    // Collect provider configuration
    let provider_config = if args.yes {
        default_provider_config()
    } else {
        prompt_provider_config()?
    };

    // Write config.json
    write_config(&config_path, &provider_config)?;

    println!();
    println!("Setup complete.");
    println!("  Config: {}", config_path.display());
    println!("  Workspace: {}", config_dir.join("workspace").display());
    println!();
    println!("Next steps:");
    println!("  1. Set your API key as an environment variable, or");
    println!("     edit {} to add it directly.", config_path.display());
    println!("  2. Start the kernel: weaver kernel start");
    println!("  3. Run: weft agent -m \"hello\"");
    println!("     (no daemon? add --local to run the agent in-process)");

    Ok(())
}

/// Resolve the configuration directory path.
fn resolve_config_dir(override_dir: Option<&str>) -> anyhow::Result<PathBuf> {
    if let Some(dir) = override_dir {
        return Ok(PathBuf::from(dir));
    }
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("could not determine home directory"))?;
    Ok(home.join(".clawft"))
}

/// Create the directory structure under the config root.
fn create_directory_structure(config_dir: &Path) -> anyhow::Result<()> {
    // Main config dir
    std::fs::create_dir_all(config_dir)?;
    println!("  Created {}", config_dir.display());

    // Workspace subdirectory
    let workspace_dir = config_dir.join("workspace");
    std::fs::create_dir_all(&workspace_dir)?;

    // Subdirs under workspace
    for subdir in WORKSPACE_DIRS {
        let path = workspace_dir.join(subdir);
        std::fs::create_dir_all(&path)?;
    }
    println!(
        "  Created workspace directories: {}",
        WORKSPACE_DIRS.join(", ")
    );

    Ok(())
}

/// Prompt the user for provider configuration interactively.
fn prompt_provider_config() -> anyhow::Result<ProviderSelection> {
    println!("Which LLM provider would you like to configure?");
    println!();
    println!("  1. OpenAI         (OPENAI_API_KEY)");
    println!("  2. Anthropic      (ANTHROPIC_API_KEY)");
    println!("  3. OpenRouter     (OPENROUTER_API_KEY)");
    println!("  4. Groq           (GROQ_API_KEY)");
    println!("  5. DeepSeek       (DEEPSEEK_API_KEY)");
    println!("  6. Custom endpoint");
    println!("  7. Skip (configure later)");
    println!();
    print!("Choice [1-7, default 1]: ");
    io::stdout().flush()?;

    let mut choice = String::new();
    io::stdin().lock().read_line(&mut choice)?;
    let choice = choice.trim();

    let selection = match choice {
        "" | "1" => ProviderSelection::preset("openai", "OPENAI_API_KEY"),
        "2" => ProviderSelection::preset("anthropic", "ANTHROPIC_API_KEY"),
        "3" => ProviderSelection::preset("openrouter", "OPENROUTER_API_KEY"),
        "4" => ProviderSelection::preset("groq", "GROQ_API_KEY"),
        "5" => ProviderSelection::preset("deepseek", "DEEPSEEK_API_KEY"),
        "6" => prompt_custom_provider()?,
        "7" => return Ok(ProviderSelection::none()),
        _ => {
            println!("Invalid choice, defaulting to OpenAI.");
            ProviderSelection::preset("openai", "OPENAI_API_KEY")
        }
    };

    // Optionally prompt for the API key value
    if selection.provider_name.is_some() {
        let env_var = selection.api_key_env.as_deref().unwrap_or("API_KEY");
        println!();
        println!(
            "You can set your API key now, or set the {} env var later.",
            env_var
        );
        // Use rpassword to suppress echo so the key is not visible in terminal
        // scrollback or screen recordings. rpassword 7 reads the controlling
        // TTY, so piped stdin is read explicitly via read_secret.
        print!("API key (leave blank to skip): ");
        io::stdout().flush()?;
        let key = read_secret().unwrap_or_default().trim().to_owned();

        if key.is_empty() {
            return Ok(selection);
        }

        return Ok(ProviderSelection {
            api_key: Some(key),
            ..selection
        });
    }

    Ok(selection)
}

/// Read a secret: no-echo from the TTY, or one line from piped stdin.
fn read_secret() -> io::Result<String> {
    if io::stdin().is_terminal() {
        rpassword::read_password()
    } else {
        rpassword::read_password_from_bufread(&mut io::stdin().lock())
    }
}

/// Prompt for a custom provider endpoint.
fn prompt_custom_provider() -> anyhow::Result<ProviderSelection> {
    print!("Provider name: ");
    io::stdout().flush()?;
    let mut name = String::new();
    io::stdin().lock().read_line(&mut name)?;
    let name = name.trim().to_owned();
    if name.is_empty() {
        anyhow::bail!("provider name cannot be empty");
    }

    print!("Base URL (e.g. https://api.example.com/v1): ");
    io::stdout().flush()?;
    let mut url = String::new();
    io::stdin().lock().read_line(&mut url)?;
    let url = url.trim().to_owned();
    if url.is_empty() {
        anyhow::bail!("base URL cannot be empty");
    }

    print!(
        "API key env var name [default: {}_API_KEY]: ",
        name.to_uppercase()
    );
    io::stdout().flush()?;
    let mut env = String::new();
    io::stdin().lock().read_line(&mut env)?;
    let env = env.trim().to_owned();
    let env = if env.is_empty() {
        format!("{}_API_KEY", name.to_uppercase())
    } else {
        env
    };

    Ok(ProviderSelection {
        provider_name: Some(name),
        api_key_env: Some(env),
        api_base: Some(url),
        api_key: None,
    })
}

/// Default provider config (OpenAI) for non-interactive mode.
fn default_provider_config() -> ProviderSelection {
    ProviderSelection::preset("openai", "OPENAI_API_KEY")
}

/// Write the config.json file.
fn write_config(path: &Path, provider: &ProviderSelection) -> anyhow::Result<()> {
    let mut providers = serde_json::Map::new();

    if let Some(ref name) = provider.provider_name {
        let mut entry = serde_json::Map::new();

        if let Some(ref key) = provider.api_key {
            entry.insert("api_key".into(), json!(key));
        }
        if let Some(ref base) = provider.api_base {
            entry.insert("api_base".into(), json!(base));
        }

        providers.insert(name.clone(), serde_json::Value::Object(entry));
    }

    let config = json!({
        "agents": {
            "defaults": {
                "model": default_model_for(provider.provider_name.as_deref()),
                "system_prompt": "You are a helpful AI assistant.",
                "context_window": 128000,
                "workspace": "~/.clawft/workspace"
            }
        },
        "providers": providers,
        "tools": {
            "exec_enabled": false,
            "exec_timeout_secs": 30,
            "allowed_paths": []
        },
        "gateway": {
            "host": "127.0.0.1",
            "port": 8080
        },
        "channels": {}
    });

    let formatted = serde_json::to_string_pretty(&config)?;
    std::fs::write(path, formatted.as_bytes())?;
    println!("  Wrote {}", path.display());

    Ok(())
}

/// Return a sensible default model for the given provider.
fn default_model_for(provider: Option<&str>) -> &'static str {
    match provider {
        Some("openai") => "openai/gpt-4o",
        Some("anthropic") => "anthropic/claude-sonnet-4-5-20250514",
        Some("openrouter") => "openrouter/anthropic/claude-sonnet-4-5-20250514",
        Some("groq") => "groq/llama-3.1-70b-versatile",
        Some("deepseek") => "deepseek/deepseek-chat",
        _ => "openai/gpt-4o",
    }
}

/// Intermediate type representing what the user chose.
struct ProviderSelection {
    provider_name: Option<String>,
    api_key_env: Option<String>,
    api_base: Option<String>,
    api_key: Option<String>,
}

impl ProviderSelection {
    fn preset(name: &str, env: &str) -> Self {
        Self {
            provider_name: Some(name.into()),
            api_key_env: Some(env.into()),
            api_base: None,
            api_key: None,
        }
    }

    fn none() -> Self {
        Self {
            provider_name: None,
            api_key_env: None,
            api_base: None,
            api_key: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn piped_secret_reads_one_line_without_newline() {
        let mut input = io::Cursor::new(b"sk-test-key\nnext line\n".to_vec());
        let key = rpassword::read_password_from_bufread(&mut input).unwrap();
        assert_eq!(key, "sk-test-key");
    }

    #[test]
    fn piped_secret_eof_is_an_error_and_blank_line_is_empty() {
        // EOF is an error (the caller maps it to "skip"); a blank line is "".
        let mut eof = io::Cursor::new(Vec::<u8>::new());
        let err = rpassword::read_password_from_bufread(&mut eof).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        let mut blank = io::Cursor::new(b"\n".to_vec());
        assert_eq!(rpassword::read_password_from_bufread(&mut blank).unwrap(), "");
    }

    #[test]
    fn resolve_config_dir_override() {
        let dir = resolve_config_dir(Some("/tmp/test-clawft")).unwrap();
        assert_eq!(dir, PathBuf::from("/tmp/test-clawft"));
    }

    #[test]
    fn resolve_config_dir_default() {
        let dir = resolve_config_dir(None).unwrap();
        assert!(dir.to_string_lossy().ends_with(".clawft"));
    }

    #[test]
    fn create_directory_structure_creates_all_dirs() {
        let tmp = tempdir("create-dirs");
        create_directory_structure(&tmp).unwrap();

        assert!(tmp.join("workspace").is_dir());
        assert!(tmp.join("workspace/sessions").is_dir());
        assert!(tmp.join("workspace/memory").is_dir());
        assert!(tmp.join("workspace/skills").is_dir());
    }

    #[test]
    fn write_config_creates_valid_json() {
        let tmp = tempdir("write-valid-json");
        let config_path = tmp.join("config.json");
        let provider = ProviderSelection::preset("openai", "OPENAI_API_KEY");

        write_config(&config_path, &provider).unwrap();

        let contents = fs::read_to_string(&config_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&contents).unwrap();

        assert!(parsed.get("agents").is_some());
        assert!(parsed.get("providers").is_some());
        assert!(parsed.get("tools").is_some());
        assert!(parsed.get("gateway").is_some());

        // Check provider was written
        let providers = parsed.get("providers").unwrap();
        assert!(providers.get("openai").is_some());
    }

    #[test]
    fn write_config_with_api_key() {
        let tmp = tempdir("write-api-key");
        let config_path = tmp.join("config.json");
        let provider = ProviderSelection {
            provider_name: Some("anthropic".into()),
            api_key_env: Some("ANTHROPIC_API_KEY".into()),
            api_base: None,
            api_key: Some("sk-ant-test-key".into()),
        };

        write_config(&config_path, &provider).unwrap();

        let contents = fs::read_to_string(&config_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&contents).unwrap();
        let anthropic = &parsed["providers"]["anthropic"];
        assert_eq!(anthropic["api_key"].as_str(), Some("sk-ant-test-key"));
    }

    #[test]
    fn write_config_with_custom_base() {
        let tmp = tempdir("write-custom-base");
        let config_path = tmp.join("config.json");
        let provider = ProviderSelection {
            provider_name: Some("custom".into()),
            api_key_env: Some("CUSTOM_API_KEY".into()),
            api_base: Some("https://my-llm.example.com/v1".into()),
            api_key: None,
        };

        write_config(&config_path, &provider).unwrap();

        let contents = fs::read_to_string(&config_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&contents).unwrap();
        let custom = &parsed["providers"]["custom"];
        assert_eq!(
            custom["api_base"].as_str(),
            Some("https://my-llm.example.com/v1")
        );
    }

    #[test]
    fn write_config_no_provider() {
        let tmp = tempdir("write-no-provider");
        let config_path = tmp.join("config.json");
        let provider = ProviderSelection::none();

        write_config(&config_path, &provider).unwrap();

        let contents = fs::read_to_string(&config_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&contents).unwrap();
        let providers = parsed.get("providers").unwrap();
        assert!(providers.as_object().unwrap().is_empty());
    }

    #[test]
    fn default_model_for_known_providers() {
        assert_eq!(default_model_for(Some("openai")), "openai/gpt-4o");
        assert_eq!(
            default_model_for(Some("anthropic")),
            "anthropic/claude-sonnet-4-5-20250514"
        );
        assert_eq!(
            default_model_for(Some("groq")),
            "groq/llama-3.1-70b-versatile"
        );
        assert_eq!(
            default_model_for(Some("deepseek")),
            "deepseek/deepseek-chat"
        );
    }

    #[test]
    fn default_model_for_unknown_provider() {
        assert_eq!(default_model_for(Some("unknown")), "openai/gpt-4o");
        assert_eq!(default_model_for(None), "openai/gpt-4o");
    }

    #[test]
    fn provider_selection_preset() {
        let s = ProviderSelection::preset("openai", "OPENAI_API_KEY");
        assert_eq!(s.provider_name.as_deref(), Some("openai"));
        assert_eq!(s.api_key_env.as_deref(), Some("OPENAI_API_KEY"));
        assert!(s.api_base.is_none());
        assert!(s.api_key.is_none());
    }

    #[test]
    fn provider_selection_none() {
        let s = ProviderSelection::none();
        assert!(s.provider_name.is_none());
        assert!(s.api_key_env.is_none());
        assert!(s.api_base.is_none());
        assert!(s.api_key.is_none());
    }

    /// Create a unique temporary directory for each test.
    fn tempdir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("clawft-onboard-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
