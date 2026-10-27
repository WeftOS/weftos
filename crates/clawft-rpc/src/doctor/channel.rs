//! Which install channel owns a binary copy.
//!
//! Evidence, strongest first: Homebrew Cellar symlink, cargo-dist receipt
//! (`~/.config/<app>/*receipt*.json`), `build.sh install` marker
//! (`~/.config/weftos/dev-install.json`), a `-dirty` build, the cargo install
//! ledger (`~/.cargo/.crates2.json`), otherwise unknown.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use super::env::DoctorEnv;

/// Install channel kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ChannelKind {
    /// Homebrew formula (binary is a Cellar symlink).
    #[serde(rename = "homebrew")]
    Homebrew,
    /// cargo-dist shell/powershell installer (has a receipt).
    #[serde(rename = "cargo-dist")]
    CargoDist,
    /// `scripts/build.sh install` (marker file or dirty build).
    #[serde(rename = "build.sh")]
    DevBuild,
    /// `cargo install` (ledger entry).
    #[serde(rename = "cargo-install")]
    CargoInstall,
    /// No evidence.
    #[serde(rename = "unknown")]
    Unknown,
}

/// Channel plus the evidence string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Channel {
    /// Kind.
    pub kind: ChannelKind,
    /// App/formula/crate name or inference note.
    pub detail: Option<String>,
}

#[derive(Debug, Default)]
struct Receipt {
    app: String,
    binaries: Vec<String>,
    prefix: PathBuf,
}

/// Channel evidence loaded once per doctor run.
#[derive(Debug, Default)]
pub struct Sources {
    receipts: Vec<Receipt>,
    dev_paths: Vec<PathBuf>,
    ledger: Vec<(String, Vec<String>)>, // (crate + version, bins)
    cargo_bin: PathBuf,
}

impl Sources {
    /// Read receipts, dev marker and cargo ledger. Missing files are fine.
    pub fn load(env: &DoctorEnv) -> Self {
        let mut s = Sources { cargo_bin: env.cargo_home.join("bin"), ..Default::default() };
        if let Ok(apps) = std::fs::read_dir(&env.config_dir) {
            for app in apps.flatten() {
                let Ok(files) = std::fs::read_dir(app.path()) else { continue };
                for f in files.flatten() {
                    let name = f.file_name().to_string_lossy().into_owned();
                    if name.contains("receipt")
                        && name.ends_with(".json")
                        && let Some(r) = parse_receipt(&f.path(), &app.file_name().to_string_lossy())
                    {
                        s.receipts.push(r);
                    }
                }
            }
        }
        s.dev_paths = read_dev_marker(&env.config_dir.join("weftos/dev-install.json"));
        s.ledger = read_ledger(&env.cargo_home.join(".crates2.json"));
        s
    }

    /// Classify one copy.
    pub fn detect(&self, path: &Path, canonical: &Path, name: &str, dirty: bool) -> Channel {
        if let Some(formula) = cellar_formula(canonical) {
            return Channel { kind: ChannelKind::Homebrew, detail: Some(formula) };
        }
        for r in &self.receipts {
            let in_prefix = [path, canonical].iter().any(|p| {
                p.parent().is_some_and(|d| d == r.prefix || d == r.prefix.join("bin"))
            });
            if in_prefix && r.binaries.iter().any(|b| b == name) {
                return Channel { kind: ChannelKind::CargoDist, detail: Some(r.app.clone()) };
            }
        }
        if self.dev_paths.iter().any(|p| p == path) {
            return Channel { kind: ChannelKind::DevBuild, detail: Some("dev-install marker".into()) };
        }
        let in_cargo = path.parent() == Some(self.cargo_bin.as_path());
        if dirty {
            return Channel { kind: ChannelKind::DevBuild, detail: Some("inferred: -dirty build".into()) };
        }
        if in_cargo
            && let Some((krate, _)) = self.ledger.iter().find(|(_, bins)| bins.iter().any(|b| b == name))
        {
            return Channel { kind: ChannelKind::CargoInstall, detail: Some(krate.clone()) };
        }
        Channel { kind: ChannelKind::Unknown, detail: None }
    }

    /// Ledger entry (`crate version`) that claims `name`, for drift notes.
    pub fn ledger_entry(&self, name: &str) -> Option<&str> {
        self.ledger
            .iter()
            .find(|(_, bins)| bins.iter().any(|b| b == name))
            .map(|(k, _)| k.as_str())
    }
}

/// `…/Cellar/<formula>/<ver>/bin/x` -> formula.
fn cellar_formula(p: &Path) -> Option<String> {
    let comps: Vec<_> = p.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    let i = comps.iter().position(|c| c == "Cellar")?;
    comps.get(i + 1).cloned()
}

fn parse_receipt(path: &Path, app_dir: &str) -> Option<Receipt> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let binaries = v
        .get("binaries")?
        .as_array()?
        .iter()
        .filter_map(|b| b.as_str().map(str::to_owned))
        .collect();
    let app = v
        .pointer("/source/app_name")
        .and_then(Value::as_str)
        .unwrap_or(app_dir)
        .to_string();
    Some(Receipt { app, binaries, prefix: PathBuf::from(v.get("install_prefix")?.as_str()?) })
}

fn read_dev_marker(path: &Path) -> Vec<PathBuf> {
    let Some(v) = std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok())
    else {
        return Vec::new();
    };
    let items: Vec<&Value> = match (&v, v.get("installs").and_then(Value::as_array)) {
        (_, Some(a)) => a.iter().collect(),
        (Value::Array(a), _) => a.iter().collect(),
        _ => vec![&v],
    };
    items
        .into_iter()
        .filter_map(|i| i.get("path").and_then(Value::as_str).map(PathBuf::from))
        .collect()
}

fn read_ledger(path: &Path) -> Vec<(String, Vec<String>)> {
    let Some(v) = std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok())
    else {
        return Vec::new();
    };
    let Some(installs) = v.get("installs").and_then(Value::as_object) else {
        return Vec::new();
    };
    installs
        .iter()
        .filter_map(|(k, v)| {
            let bins = v.get("bins")?.as_array()?.iter().filter_map(|b| b.as_str().map(str::to_owned)).collect();
            // key: "clawft-cli 0.6.20 (path+file:///…)" -> "clawft-cli 0.6.20"
            let key = k.split(" (").next().unwrap_or(k).to_string();
            Some((key, bins))
        })
        .collect()
}

/// One-line command that updates/replaces a copy through its own channel.
pub fn remedy_for(ch: &Channel, name: &str) -> String {
    match ch.kind {
        ChannelKind::Homebrew => {
            let formula = ch.detail.clone().unwrap_or_else(|| brew_formula(name).into());
            format!("brew upgrade weave-logic-ai/tap/{formula}")
        }
        ChannelKind::CargoDist => {
            let app = ch.detail.clone().unwrap_or_else(|| "weftos".into());
            format!(
                "curl --proto '=https' --tlsv1.2 -LsSf https://github.com/WeftOS/weftos/releases/latest/download/{app}-installer.sh | sh"
            )
        }
        ChannelKind::DevBuild => "scripts/build.sh install   (from a clean checkout)".into(),
        ChannelKind::CargoInstall => "scripts/build.sh install, or re-run the release installer".into(),
        ChannelKind::Unknown => "weaver update   (or reinstall via the release installer)".into(),
    }
}

/// Homebrew formula that ships a binary.
pub fn brew_formula(name: &str) -> &'static str {
    match name {
        "weft" => "clawft-cli",
        "weaver" => "clawft-weave",
        _ => "weftos",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::env::test_env;

    fn write(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    #[test]
    fn receipt_parsing_and_match() {
        let d = tempfile::tempdir().unwrap();
        let env = test_env(d.path());
        write(
            &env.config_dir.join("weftos/weftos-receipt.json"),
            r#"{"binaries":["weftos"],"install_prefix":"/x/.cargo","source":{"app_name":"weftos"}}"#,
        );
        let s = Sources::load(&env);
        let p = Path::new("/x/.cargo/bin/weftos");
        let c = s.detect(p, p, "weftos", false);
        assert_eq!(c.kind, ChannelKind::CargoDist);
        assert_eq!(c.detail.as_deref(), Some("weftos"));
        // A binary the receipt does not list is not owned by it.
        assert_eq!(s.detect(Path::new("/x/.cargo/bin/weft"), Path::new("/x/.cargo/bin/weft"), "weft", false).kind, ChannelKind::Unknown);
    }

    #[test]
    fn homebrew_cellar_symlink_target() {
        let s = Sources::default();
        let c = s.detect(
            Path::new("/opt/homebrew/bin/weft"),
            Path::new("/opt/homebrew/Cellar/clawft-cli/0.8.1/bin/weft"),
            "weft",
            false,
        );
        assert_eq!(c.kind, ChannelKind::Homebrew);
        assert_eq!(remedy_for(&c, "weft"), "brew upgrade weave-logic-ai/tap/clawft-cli");
    }

    #[test]
    fn dirty_is_dev_and_ledger_is_cargo_install() {
        let d = tempfile::tempdir().unwrap();
        let env = test_env(d.path());
        write(
            &env.cargo_home.join(".crates2.json"),
            r#"{"installs":{"clawft-cli 0.6.20 (path+file:///w)":{"bins":["weft"]}}}"#,
        );
        let s = Sources::load(&env);
        let p = env.cargo_home.join("bin/weft");
        assert_eq!(s.detect(&p, &p, "weft", true).kind, ChannelKind::DevBuild);
        assert_eq!(s.detect(&p, &p, "weft", false).kind, ChannelKind::CargoInstall);
        assert_eq!(s.ledger_entry("weft"), Some("clawft-cli 0.6.20"));
    }

    #[test]
    fn dev_marker_wins_over_ledger() {
        let d = tempfile::tempdir().unwrap();
        let env = test_env(d.path());
        write(&env.config_dir.join("weftos/dev-install.json"), r#"{"installs":[{"path":"/a/bin/weft","sha":"x"}]}"#);
        let s = Sources::load(&env);
        let c = s.detect(Path::new("/a/bin/weft"), Path::new("/a/bin/weft"), "weft", false);
        assert_eq!(c.kind, ChannelKind::DevBuild);
    }
}
