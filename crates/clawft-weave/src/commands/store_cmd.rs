//! `weaver store` (ADR-126 S1): the mesh object store.
//!
//! - `weaver store ls [<name>] [--prefix P] [--json]`: every store, or one store's grants and objects.
//! - `weaver store create <name> [--admin P].. [--write P].. [--read P]..`
//! - `weaver store grant <name> <principal> <read|write|admin>`; `--off` / `--on` switch an
//!   existing grant without losing it.
//! - `weaver store revoke <name> <principal>`: the principal's next request is refused.
//! - `weaver store promote <worktree store> --prefix P..  [--archive-rest]`: into the project store.
//! - `weaver store archive <name> [--delete]`: copy to the archive bucket, close, optionally delete.
//! - `weaver store put|get <name> <key> --as <principal>`: a governed request as that principal.
//!
//! Names are `weftos://<mesh>/nodes/<id>/store`, `.../projects/<ULID>/store`,
//! `.../worktrees/<slug>/store`, `.../releases/<channel>/store`. Principals are
//! `node:<id>`, `member:<id>`, `agent:<id>` or `cog:<id>`. Every verb is Admin.

use clap::{Parser, Subcommand};
use serde_json::{Value, json};

use crate::protocol;

/// Store subcommand.
#[derive(Parser)]
#[command(about = "The mesh object store: ls, create, grant, revoke, promote, archive")]
pub struct StoreArgs {
    #[command(subcommand)]
    pub action: StoreAction,
}

/// Store subcommands.
#[derive(Subcommand)]
pub enum StoreAction {
    /// List stores, or one store's grants and objects.
    Ls {
        /// Store name; without it, every store.
        name: Option<String>,
        /// Only objects under this prefix.
        #[arg(long)]
        prefix: Option<String>,
        /// Print the raw JSON.
        #[arg(long)]
        json: bool,
    },
    /// Create a store (the daemon creates its node store itself).
    Create {
        name: String,
        /// Principals with admin access (repeatable).
        #[arg(long = "admin")]
        admin: Vec<String>,
        /// Principals with write access (repeatable).
        #[arg(long = "write")]
        write: Vec<String>,
        /// Principals with read access (repeatable).
        #[arg(long = "read")]
        read: Vec<String>,
    },
    /// Grant access, or switch a grant off or on.
    Grant {
        name: String,
        principal: String,
        /// read, write or admin.
        #[arg(required_unless_present_any = ["off", "on"])]
        access: Option<String>,
        /// Disable the grant (kept, allows nothing).
        #[arg(long, conflicts_with = "on")]
        off: bool,
        /// Enable the grant.
        #[arg(long)]
        on: bool,
    },
    /// Remove a grant.
    Revoke { name: String, principal: String },
    /// Copy a worktree store's promotable paths into its project store.
    Promote {
        name: String,
        /// Promotable key prefix (repeatable, at least one).
        #[arg(long = "prefix", required = true)]
        prefix: Vec<String>,
        /// Copy everything else to the archive bucket.
        #[arg(long)]
        archive_rest: bool,
    },
    /// Copy a store to the archive bucket and close it.
    Archive {
        name: String,
        /// Then delete the bucket and forget the store.
        #[arg(long)]
        delete: bool,
    },
    /// Write a text object as a principal.
    Put {
        name: String,
        key: String,
        /// Content.
        text: String,
        /// The requesting principal.
        #[arg(long = "as")]
        principal: String,
    },
    /// Worktree stores and sharing (ADR-126 S2); the verbs `scripts/worktree-store.sh` calls:
    /// create, buckets, publish, fetch, claim, promote, follow, search, leftovers, sync-docs, overlaps.
    Worktree {
        verb: String,
        /// `--key value` pairs (and the flags `--json`, `--force`, `--confirmed`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        rest: Vec<String>,
    },
    /// Read an object as a principal.
    Get {
        name: String,
        key: String,
        #[arg(long = "as")]
        principal: String,
    },
}

fn cell(v: &Value, k: &str) -> String {
    match &v[k] {
        Value::Null => "-".into(),
        Value::String(s) => s.clone(),
        o => o.to_string(),
    }
}

fn grants_lines(info: &Value) -> String {
    let mut out = String::new();
    for g in info["grants"].as_array().into_iter().flatten() {
        let on = if g["enabled"] == false { " (disabled)" } else { "" };
        out += &format!("    {:<28} {}{on}\n", format!("{}:{}", cell(&g["principal"], "kind"), cell(&g["principal"], "id")), cell(g, "access"));
    }
    out
}

/// `store.ls` as a table.
pub fn render_ls(v: &Value) -> String {
    if let Some(info) = v.get("store").filter(|s| s.is_object()) {
        let mut out = format!("{}  bucket {}{}\n  grants:\n", cell(info, "name"), cell(info, "bucket"), if info["archived"] == true { "  ARCHIVED" } else { "" });
        out += &grants_lines(info);
        let objs = v["objects"].as_array().cloned().unwrap_or_default();
        out += &format!("  objects: {}\n", objs.len());
        for o in objs {
            out += &format!("    {:>10}  {}\n", cell(&o, "size"), cell(&o, "key"));
        }
        return out;
    }
    let stores = v["stores"].as_array().cloned().unwrap_or_default();
    if stores.is_empty() {
        return "no stores\n".into();
    }
    let mut out = format!("{:<60} {:<8} {:>8} {:>12} {:>6}\n", "NAME", "KIND", "OBJECTS", "BYTES", "GRANTS");
    for s in stores {
        let n = s["grants"].as_array().map_or(0, Vec::len);
        let tag = if s["archived"] == true { " [archived]" } else { "" };
        out += &format!("{:<60} {:<8} {:>8} {:>12} {:>6}{tag}\n", cell(&s, "name"), cell(&s, "kind"), cell(&s, "objects"), cell(&s, "bytes"), n);
    }
    out
}

async fn call(method: &str, params: Value) -> anyhow::Result<Value> {
    let mut client = clawft_rpc::connect_or_bail().await?;
    let resp = client.call(protocol::Request::with_params(method, params)).await?;
    if !resp.ok {
        anyhow::bail!(resp.error.unwrap_or_else(|| "unknown error".into()));
    }
    Ok(resp.result.unwrap_or_default())
}

const WT_FLAGS: &[&str] = &["json", "force", "confirmed", "full"];

/// `--key value` pairs to the daemon's params: hyphens become underscores, `--json`, `--force` and
/// `--confirmed` are flags. The acting agent and host come from the environment.
pub fn worktree_params(rest: &[String], agent: &str, host: &str) -> anyhow::Result<Value> {
    let mut m = serde_json::Map::new();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        let Some(k) = a.strip_prefix("--").filter(|k| !k.is_empty()) else {
            anyhow::bail!("unexpected argument '{a}' (expected --key value)");
        };
        let k = k.replace('-', "_");
        if WT_FLAGS.contains(&k.as_str()) {
            m.insert(k, Value::Bool(true));
        } else {
            let v = it.next().ok_or_else(|| anyhow::anyhow!("--{k} needs a value"))?;
            m.insert(k, Value::String(v.clone()));
        }
    }
    m.entry("agent").or_insert_with(|| Value::String(agent.into()));
    m.entry("host").or_insert_with(|| Value::String(host.into()));
    Ok(Value::Object(m))
}

fn wt_lines(verb: &str, v: &Value) -> Option<String> {
    let line = |w: &Value| format!("{} [{}] {} {}", cell(w, "slug"), cell(w, "state"), cell(w, "branch"), cell(w, "writer"));
    match verb {
        "buckets" => Some(format!("{} {}\n", cell(v, "worktree"), cell(v, "project"))),
        "follow" => {
            let c = v["changes"].as_array()?;
            Some(if c.is_empty() {
                "no changes\n".into()
            } else {
                c.iter().map(|x| format!("{:<14} {}\n", cell(x, "change"), if x["worktree"].is_object() { line(&x["worktree"]) } else { cell(x, "slug") })).collect()
            })
        }
        "search" => {
            let h = v["hits"].as_array()?;
            let mut out: String = h.iter().map(|x| format!("{}  {}  {}\n", cell(x, "slug"), cell(x, "path"), cell(x, "title"))).collect();
            if h.is_empty() {
                out = "no matches\n".into();
            }
            for k in ["unreadable", "unreadable_entries"] {
                for x in v[k].as_array().into_iter().flatten().filter_map(Value::as_str) {
                    out.push_str(&format!("(could not read {} {x})\n", if k == "unreadable" { "worktree" } else { "registry entry" }));
                }
            }
            Some(out)
        }
        "leftovers" => {
            let l = v["leftovers"].as_array()?;
            Some(if l.is_empty() {
                "no leftovers\n".into()
            } else {
                l.iter().map(|x| format!("{:<18} idle {} h  {}\n", cell(x, "why"), x["idle_ms"].as_u64().unwrap_or(0) / 3_600_000, line(&x["worktree"]))).collect()
            })
        }
        _ => None,
    }
}

/// This machine's name for claim identities: the kernel's hostname file, `/etc/hostname`, then the
/// shell variables, then (macOS, which has none of those) the system preferences, whose
/// `LocalHostName` is the Bonjour name. Reading a file avoids a process or an FFI call.
fn local_host() -> Option<String> {
    let clean = |s: &str| Some(s.trim().to_owned()).filter(|h| !h.is_empty() && h != "unknown" && h != "localhost");
    for f in ["/proc/sys/kernel/hostname", "/etc/hostname"] {
        if let Some(h) = std::fs::read_to_string(f).ok().and_then(|s| clean(&s)) {
            return Some(h);
        }
    }
    for v in ["HOSTNAME", "HOST"] {
        if let Some(h) = std::env::var(v).ok().and_then(|s| clean(&s)) {
            return Some(h);
        }
    }
    let plist = std::fs::read_to_string("/Library/Preferences/SystemConfiguration/preferences.plist").ok()?;
    ["LocalHostName", "HostName", "ComputerName"].iter().find_map(|k| plist_string(&plist, k).and_then(|s| clean(&s)))
}

/// The `<string>` that follows `<key>name</key>` in an XML property list (first match).
fn plist_string(plist: &str, name: &str) -> Option<String> {
    let after = &plist[plist.find(&format!("<key>{name}</key>"))? + name.len() + 11..];
    let start = after.find("<string>")? + 8;
    let end = after[start..].find("</string>")?;
    Some(after[start..start + end].to_owned())
}

/// Run the store subcommand.
pub async fn run(args: StoreArgs) -> anyhow::Result<()> {
    match args.action {
        StoreAction::Ls { name, prefix, json: j } => {
            let v = call("store.ls", json!({"name": name, "prefix": prefix})).await?;
            if j {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                print!("{}", render_ls(&v));
            }
        }
        StoreAction::Create { name, admin, write, read } => {
            let v = call("store.create", json!({"name": name, "admin": admin, "write": write, "read": read})).await?;
            println!("created {} (bucket {})", cell(&v, "name"), cell(&v, "bucket"));
        }
        StoreAction::Grant { name, principal, access, off, on } => {
            let enabled = if off { Some(false) } else if on { Some(true) } else { None };
            let v = call("store.grant", json!({"name": name, "principal": principal, "access": access, "enabled": enabled})).await?;
            match enabled {
                Some(e) if access.is_none() => println!("{principal} on {name}: {}", if e { "enabled" } else { "disabled: its next request is refused" }),
                _ => println!("{principal} on {name}: {}{}", cell(&v, "access"), if enabled == Some(false) { " (disabled)" } else { "" }),
            }
        }
        StoreAction::Revoke { name, principal } => {
            call("store.revoke", json!({"name": name, "principal": principal})).await?;
            println!("revoked {principal} on {name}: its next request is refused");
        }
        StoreAction::Promote { name, prefix, archive_rest } => {
            let v = call("store.promote", json!({"name": name, "prefixes": prefix, "archive_rest": archive_rest})).await?;
            println!("promoted {}, unchanged {}, left {}, archived {}", cell(&v, "promoted"), cell(&v, "unchanged"), cell(&v, "left"), cell(&v, "archived"));
            for k in v["replaced"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                println!("  replaced in the project store: {k}");
            }
        }
        StoreAction::Archive { name, delete } => {
            let v = call("store.archive", json!({"name": name, "delete": delete})).await?;
            println!("archived {} object(s) of {name}{}", cell(&v, "archived_objects"), if delete { "; bucket deleted" } else { "; store closed" });
        }
        StoreAction::Put { name, key, text, principal } => {
            let v = call("store.put", json!({"name": name, "key": key, "text": text, "principal": principal})).await?;
            println!("wrote {} ({} bytes)", cell(&v, "key"), cell(&v, "size"));
        }
        StoreAction::Worktree { verb, rest } => {
            let agent = std::env::var("WEFTOS_AGENT").or_else(|_| std::env::var("USER")).unwrap_or_else(|_| "agent".into());
            // The host half of a claim identity must tell machines apart; never a shared fallback.
            let host = match local_host() {
                Some(h) => h,
                None if rest.iter().any(|a| a == "--host") => String::new(),
                None => anyhow::bail!("cannot tell this machine's host name; pass --host <name>"),
            };
            let as_json = rest.iter().any(|a| a == "--json");
            let params = worktree_params(&rest, &agent, &host)?;
            let v = call(&format!("store.worktree.{verb}"), params).await?;
            match (as_json, wt_lines(&verb, &v)) {
                (false, Some(t)) => print!("{t}"),
                _ => println!("{}", serde_json::to_string_pretty(&v)?),
            }
        }
        StoreAction::Get { name, key, principal } => {
            let v = call("store.get", json!({"name": name, "key": key, "principal": principal})).await?;
            match v["text"].as_str() {
                Some(t) => print!("{t}"),
                None => anyhow::bail!("{} bytes of binary data; use --json via store ls to inspect", cell(&v, "size")),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_host_names_are_read_without_a_process() {
        let p = "<dict><key>System</key><dict><key>Network</key><dict><key>HostNames</key><dict>\n<key>LocalHostName</key>\n\t<string>mac-studio</string></dict></dict><key>System</key><dict><key>ComputerName</key><string>Mac Studio</string></dict></dict></dict>";
        assert_eq!(plist_string(p, "LocalHostName").as_deref(), Some("mac-studio"));
        assert_eq!(plist_string(p, "ComputerName").as_deref(), Some("Mac Studio"));
        assert_eq!(plist_string(p, "HostName"), None);
        assert_eq!(plist_string("", "LocalHostName"), None);
    }

    #[test]
    fn this_machine_has_a_host_name_that_is_not_the_shared_fallback() {
        // On a Mac with no HOSTNAME exported this used to be "unknown".
        if let Some(h) = local_host() {
            assert!(!h.is_empty() && h != "unknown");
        }
    }

    #[test]
    fn verbs_parse_and_required_args_are_enforced() {
        let p = |a: &[&str]| StoreArgs::try_parse_from(std::iter::once("store").chain(a.iter().copied()));
        assert!(p(&["ls"]).is_ok() && p(&["ls", "weftos://m/nodes/a/store", "--json"]).is_ok());
        assert!(p(&["grant", "n", "cog:c", "write"]).is_ok() && p(&["grant", "n", "cog:c", "--off"]).is_ok());
        assert!(p(&["grant", "n", "cog:c"]).is_err(), "needs access or --on/--off");
        assert!(p(&["grant", "n", "cog:c", "--on", "--off"]).is_err());
        assert!(p(&["promote", "n"]).is_err() && p(&["promote", "n", "--prefix", "assets/"]).is_ok());
        assert!(p(&["archive", "n", "--delete"]).is_ok() && p(&["revoke", "n", "cog:c"]).is_ok());
        assert!(p(&["put", "n", "k", "v"]).is_err() && p(&["put", "n", "k", "v", "--as", "cog:c"]).is_ok());
        assert!(p(&["create", "n", "--write", "a", "--write", "b"]).is_ok());
    }

    #[test]
    fn worktree_args_become_params() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let p = worktree_params(&a(&["--project", "01ABC", "--paths-z", "/t/p.z", "--force", "--json"]), "ag", "h1").unwrap();
        assert_eq!((p["project"].as_str(), p["paths_z"].as_str(), p["force"].as_bool(), p["agent"].as_str()), (Some("01ABC"), Some("/t/p.z"), Some(true), Some("ag")));
        assert!(worktree_params(&a(&["--project"]), "a", "h").is_err(), "value required");
        assert!(worktree_params(&a(&["stray"]), "a", "h").is_err());
        assert_eq!(wt_lines("buckets", &json!({"worktree": "w-1", "project": "p-1"})).unwrap(), "w-1 p-1\n");
        assert!(StoreArgs::try_parse_from(["store", "worktree", "publish", "--project", "X", "--changes-z", "/f"]).is_ok());
    }

    #[test]
    fn ls_renders_stores_and_one_store() {
        let v = json!({"stores": [{"name": "weftos://m/nodes/a/store", "kind": "node", "objects": 2, "bytes": 10, "grants": [{}], "archived": false}]});
        let t = render_ls(&v);
        assert!(t.contains("weftos://m/nodes/a/store") && t.contains("node"), "{t}");
        assert!(render_ls(&json!({"stores": []})).contains("no stores"));
        let one = json!({"store": {"name": "n", "bucket": "n-a", "archived": false,
            "grants": [{"principal": {"kind": "cog", "id": "c1"}, "access": "write", "enabled": false}]},
            "objects": [{"key": "k", "size": 3}]});
        let t = render_ls(&one);
        assert!(t.contains("cog:c1") && t.contains("(disabled)") && t.contains("objects: 1"), "{t}");
    }
}
