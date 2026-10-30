//! Language server configurations for common languages.

use serde::{Deserialize, Serialize};

/// Configuration for a language server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LanguageConfig {
    /// Language identifier, e.g. `rust`.
    pub name: String,
    /// Executable that launches the language server.
    pub command: String,
    /// Arguments passed to `command`.
    pub args: Vec<String>,
    /// File extensions (without the dot) this server handles.
    pub extensions: Vec<String>,
    /// Options sent to the server in the LSP `initialize` request.
    pub initialization_options: serde_json::Value,
}

impl LanguageConfig {
    /// Preset for `rust-analyzer`.
    pub fn rust() -> Self {
        Self {
            name: "rust".into(),
            command: "rust-analyzer".into(),
            args: vec![],
            extensions: vec!["rs".into()],
            initialization_options: serde_json::json!({}),
        }
    }

    /// Preset for `typescript-language-server --stdio`, covering TypeScript and JavaScript.
    pub fn typescript() -> Self {
        Self {
            name: "typescript".into(),
            command: "typescript-language-server".into(),
            args: vec!["--stdio".into()],
            extensions: vec!["ts".into(), "tsx".into(), "js".into(), "jsx".into()],
            initialization_options: serde_json::json!({}),
        }
    }

    /// Preset for `pylsp`.
    pub fn python() -> Self {
        Self {
            name: "python".into(),
            command: "pylsp".into(),
            args: vec![],
            extensions: vec!["py".into()],
            initialization_options: serde_json::json!({}),
        }
    }

    /// Preset for `gopls serve`.
    pub fn go() -> Self {
        Self {
            name: "go".into(),
            command: "gopls".into(),
            args: vec!["serve".into()],
            extensions: vec!["go".into()],
            initialization_options: serde_json::json!({}),
        }
    }

    /// Detect the appropriate config from file extensions in a directory.
    pub fn detect(path: &std::path::Path) -> Vec<Self> {
        let mut configs = Vec::new();
        let mut has_rs = false;
        let mut has_ts = false;
        let mut has_py = false;
        let mut has_go = false;

        if let Ok(entries) = walkdir(path) {
            for ext in entries {
                match ext.as_str() {
                    "rs" => has_rs = true,
                    "ts" | "tsx" | "js" | "jsx" => has_ts = true,
                    "py" => has_py = true,
                    "go" => has_go = true,
                    _ => {}
                }
            }
        }

        if has_rs {
            configs.push(Self::rust());
        }
        if has_ts {
            configs.push(Self::typescript());
        }
        if has_py {
            configs.push(Self::python());
        }
        if has_go {
            configs.push(Self::go());
        }

        configs
    }
}

fn walkdir(path: &std::path::Path) -> Result<Vec<String>, std::io::Error> {
    let mut exts = Vec::new();
    collect_extensions(path, &mut exts, 3)?; // only 3 levels deep for detection
    Ok(exts)
}

fn collect_extensions(
    dir: &std::path::Path,
    exts: &mut Vec<String>,
    depth: usize,
) -> Result<(), std::io::Error> {
    if depth == 0 {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') || name_str == "target" || name_str == "node_modules" {
            continue;
        }
        if path.is_dir() {
            collect_extensions(&path, exts, depth - 1)?;
        } else if let Some(ext) = path.extension().and_then(|e| e.to_str())
            && !exts.contains(&ext.to_string())
        {
            exts.push(ext.to_string());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_preset() {
        let c = LanguageConfig::rust();
        assert_eq!(c.name, "rust");
        assert_eq!(c.command, "rust-analyzer");
        assert!(c.args.is_empty());
        assert!(c.extensions.contains(&"rs".to_string()));
    }

    #[test]
    fn typescript_preset() {
        let c = LanguageConfig::typescript();
        assert_eq!(c.name, "typescript");
        assert_eq!(c.command, "typescript-language-server");
        assert_eq!(c.args, vec!["--stdio".to_string()]);
        assert!(c.extensions.contains(&"ts".to_string()));
    }

    #[test]
    fn python_preset() {
        let c = LanguageConfig::python();
        assert_eq!(c.name, "python");
        assert_eq!(c.command, "pylsp");
        assert!(c.args.is_empty());
        assert!(c.extensions.contains(&"py".to_string()));
    }

    #[test]
    fn go_preset() {
        let c = LanguageConfig::go();
        assert_eq!(c.name, "go");
        assert_eq!(c.command, "gopls");
        assert_eq!(c.args, vec!["serve".to_string()]);
        assert!(c.extensions.contains(&"go".to_string()));
    }
}
