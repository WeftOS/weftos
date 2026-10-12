//! `weaver versions` (ADR-121 O1): the version index of first-class objects.
//!
//! - `weaver versions list [--kind agents]`: every recorded version.
//! - `weaver versions tree <name>`: the version tree of one object, including
//!   the objects derived from it.
//! - `weaver versions deps <name>`: what the object depends on and what
//!   depends on it.
//! - `weaver versions import-catalog [--catalog agents/catalog.json]`: record a
//!   generated catalog into the local index (a local file write; the other
//!   three are read-only RPCs).

use clap::{Parser, Subcommand};
use comfy_table::{Table, presets};
use serde_json::{Value, json};

use crate::protocol;

#[derive(Parser)]
#[command(about = "Version index: history, version tree and dependencies of agents, teams and cogs")]
pub struct VersionsArgs {
    #[command(subcommand)]
    pub action: VersionsAction,
}

#[derive(Subcommand)]
pub enum VersionsAction {
    /// List every recorded version of a kind.
    List {
        #[arg(long, default_value = "agents")]
        kind: String,
        /// Print the raw JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show the version tree of one object (versions derived from it nest under them).
    Tree {
        name: String,
        #[arg(long, default_value = "agents")]
        kind: String,
        #[arg(long)]
        json: bool,
    },
    /// Show what an object depends on and what depends on it.
    Deps {
        name: String,
        #[arg(long, default_value = "agents")]
        kind: String,
        #[arg(long)]
        json: bool,
    },
    /// Record a generated agents/catalog.json into the local index.
    ImportCatalog {
        #[arg(long, default_value = "agents/catalog.json")]
        catalog: std::path::PathBuf,
        #[arg(long, default_value = "agents")]
        kind: String,
    },
}

fn short(d: &str) -> String {
    d.strip_prefix("sha256:").map_or_else(|| d.to_owned(), |h| format!("sha256:{}", &h[..h.len().min(12)]))
}

async fn call(method: &str, params: Value) -> anyhow::Result<Value> {
    let mut client = clawft_rpc::connect_or_bail().await?;
    let resp = client.call(protocol::Request::with_params(method, params)).await?;
    if !resp.ok {
        anyhow::bail!(resp.error.unwrap_or_else(|| "unknown error".into()));
    }
    Ok(resp.result.unwrap_or_default())
}

pub fn render_list(doc: &Value) -> String {
    let mut t = Table::new();
    t.load_preset(presets::UTF8_FULL_CONDENSED);
    t.set_header(["OBJECT", "VERSION", "DIGEST", "PARENT", "DEPS"]);
    for r in doc["versions"].as_array().into_iter().flatten() {
        t.add_row([
            r["object"].as_str().unwrap_or("?").to_owned(),
            r["version"].as_str().unwrap_or("?").to_owned(),
            short(r["digest"].as_str().unwrap_or("?")),
            r["parent"].as_str().map_or_else(|| "-".to_owned(), short),
            r["dependencies"].as_array().map_or(0, Vec::len).to_string(),
        ]);
    }
    format!("{t}\n")
}

pub fn render_tree(nodes: &Value, depth: usize, out: &mut String) {
    for n in nodes.as_array().into_iter().flatten() {
        let r = &n["record"];
        out.push_str(&format!(
            "{}{} {} {}\n",
            "  ".repeat(depth),
            r["object"].as_str().unwrap_or("?"),
            r["version"].as_str().unwrap_or("?"),
            short(r["digest"].as_str().unwrap_or("?"))
        ));
        render_tree(&n["children"], depth + 1, out);
    }
}

pub fn render_deps(doc: &Value) -> String {
    let mut out = String::new();
    let v = &doc["deps"];
    out.push_str(&format!("{} depends on:\n", v["object"].as_str().unwrap_or("?")));
    let deps = v["depends_on"].as_array().cloned().unwrap_or_default();
    if deps.is_empty() {
        out.push_str("  (nothing)\n");
    }
    for d in &deps {
        let pin = d["digest"].as_str().map_or_else(String::new, |x| format!(" {}", short(x)));
        out.push_str(&format!("  {} {}{pin}\n", d["kind"].as_str().unwrap_or("?"), d["name"].as_str().unwrap_or("?")));
    }
    out.push_str("depended on by:\n");
    let rev = v["dependents"].as_array().cloned().unwrap_or_default();
    if rev.is_empty() {
        out.push_str("  (nothing)\n");
    }
    for d in &rev {
        out.push_str(&format!(
            "  {}/{} {}\n",
            d["kind"].as_str().unwrap_or("?"),
            d["object"].as_str().unwrap_or("?"),
            d["version"].as_str().unwrap_or("?")
        ));
    }
    out
}

fn print_or_json(doc: &Value, json: bool, text: impl FnOnce(&Value) -> String) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(doc)?);
    } else {
        print!("{}", text(doc));
    }
    Ok(())
}

pub async fn run(args: VersionsArgs) -> anyhow::Result<()> {
    match args.action {
        VersionsAction::List { kind, json } => {
            let doc = call("versions.list", json!({ "kind": kind })).await?;
            print_or_json(&doc, json, render_list)
        }
        VersionsAction::Tree { name, kind, json } => {
            let doc = call("versions.tree", json!({ "kind": kind, "name": name })).await?;
            print_or_json(&doc, json, |d| {
                let mut s = String::new();
                render_tree(&d["tree"], 0, &mut s);
                s
            })
        }
        VersionsAction::Deps { name, kind, json } => {
            let doc = call("versions.deps", json!({ "kind": kind, "name": name })).await?;
            print_or_json(&doc, json, render_deps)
        }
        VersionsAction::ImportCatalog { catalog, kind } => {
            let text = std::fs::read_to_string(&catalog).map_err(|e| anyhow::anyhow!("{}: {e}", catalog.display()))?;
            let doc: Value = serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", catalog.display()))?;
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            let dir = clawft_rpc::runtime_dir().join("versions");
            let r = clawft_versions::VersionsIndex::new(&dir).import_catalog(&kind, &doc, now)?;
            println!("recorded {} new version(s), {} already recorded ({})", r.appended, r.already_recorded, dir.display());
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_name_version_and_short_digest() {
        let doc = json!({ "versions": [{ "object": "steward", "version": "0.8.4", "digest": format!("sha256:{}", "ab".repeat(32)), "parent": null, "dependencies": [] }] });
        let s = render_list(&doc);
        assert!(s.contains("steward") && s.contains("sha256:abababababab") && !s.contains(&"ab".repeat(32)));
    }

    #[test]
    fn tree_nests_children_and_deps_say_nothing_explicitly() {
        let d = format!("sha256:{}", "0".repeat(64));
        let rec = |o: &str| json!({ "object": o, "version": "1", "digest": d });
        let tree = json!([{ "record": rec("a"), "children": [{ "record": rec("b"), "children": [] }] }]);
        let mut s = String::new();
        render_tree(&tree, 0, &mut s);
        assert!(s.starts_with("a 1 ") && s.contains("\n  b 1 "));
        let deps = render_deps(&json!({ "deps": { "object": "a", "depends_on": [], "dependents": [] } }));
        assert_eq!(deps.matches("(nothing)").count(), 2);
    }
}
