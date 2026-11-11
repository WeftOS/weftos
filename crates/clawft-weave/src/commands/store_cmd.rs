//! `weaver store` (ADR-126 S1): the mesh object store.
//!
//! - `weaver store ls [<name>] [--prefix P] [--json]`: every store, or one store's grants and objects.
//! - `weaver store create <name> [--admin P].. [--write P].. [--read P]..`
//! - `weaver store grant <name> <principal> <read|append|sweep|write|admin>`; `--off` / `--on` switch an
//!   existing grant without losing it.
//! - `weaver store revoke <name> <principal>`: the principal's next request is refused.
//! - `weaver store promote <worktree store> --prefix P..  [--archive-rest]`: into the project store.
//! - `weaver store archive <name> [--delete]`: copy to the archive bucket, close, optionally delete.
//! - `weaver store archive-gc [--node-days N] [--project-days N] [--release-days N] [--confirmed]`: expire old
//!   archives of node, project and release stores; a dry run without `--confirmed`.
//! - `weaver store put|get <name> <key> --as <principal>`: a governed request as that principal.
//! - `weaver store drive materialise|sync-down|sync-up|resolve|watch ...`: ADR-131 prototype.
//! - `weaver store view open|status|refresh|publish|resolve|renew|close ...`: the scoped view
//!   (ADR-126 E-1); flags in `store_view_args`.
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
        /// read, append, sweep, write or admin.
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
    /// Delete old archives of node, project and release stores (a dry run unless --confirmed).
    /// With no --*-days nothing expires. Worktree archives are `store worktree gc`'s.
    ArchiveGc {
        /// Delete node-store archives older than this many days.
        #[arg(long)]
        node_days: Option<u64>,
        #[arg(long)]
        project_days: Option<u64>,
        #[arg(long)]
        release_days: Option<u64>,
        /// Actually delete; without it, only report.
        #[arg(long)]
        confirmed: bool,
    },
    /// How long one store's archives are kept, over its kind's limit: --days N, --forever, or
    /// neither to clear the override. Not for worktree stores.
    ArchiveKeep {
        name: String,
        #[arg(long, conflicts_with = "forever")]
        days: Option<u64>,
        #[arg(long)]
        forever: bool,
    },
    /// Write a text object as a principal.
    Put {
        name: String,
        key: String,
        /// Content.
        text: String,
        /// The requesting principal. Default: `$WEAVER_STORE_AS`.
        #[arg(long = "as")]
        principal: Option<String>,
    },
    /// Worktree stores and sharing (ADR-126 S2); the verbs `scripts/worktree-store.sh` calls:
    /// create, buckets, publish, fetch, claim, promote, follow, search, leftovers, sync-docs, overlaps.
    Worktree {
        verb: String,
        /// `--key value` pairs (and the flags `--json`, `--force`, `--confirmed`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        rest: Vec<String>,
    },
    /// Materialise a store prefix as a local tree and keep it in sync (ADR-131 prototype):
    /// materialise, sync-down, sync-up, resolve, watch. `watch` is one scan per call.
    Drive {
        verb: String,
        /// `--key value` pairs (and the flags `--json`, `--read-only`). `materialise` takes
        /// `--name --dest [--prefix]`; the others take `--root`, and `--name` for sync-up and resolve.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        rest: Vec<String>,
    },
    /// A scoped view of a store for one sandboxed session (ADR-126 E-1): open, status, refresh,
    /// publish, resolve, renew, close. `open` takes `--name --root` and `--spec <file>` or
    /// `--include P[=AT]`/`--work P`; `close` takes `--publish` or `--discard`.
    View {
        verb: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        rest: Vec<String>,
    },
    /// Release stores: CI publishes a release once (ADR-126 S4).
    Release(crate::commands::release_cmd::ReleaseArgs),
    /// Read an object as a principal.
    Get {
        name: String,
        key: String,
        /// The requesting principal. Default: `$WEAVER_STORE_AS`.
        #[arg(long = "as")]
        principal: Option<String>,
    },
    /// Per-client-account store (ADR-126 S5).
    Client(super::store_client_cmd::StoreClientArgs),
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

const WT_FLAGS: &[&str] = &["json", "force", "confirmed", "full", "read_only", "dry_run"];

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
            if matches!(k.as_str(), "epoch" | "max_bytes" | "quota_bytes" | "keep_days" | "archive_keep_days") {
                let n: u64 = v.parse().map_err(|_| anyhow::anyhow!("--{} needs a whole number", k.replace('_', "-")))?;
                m.insert(k, Value::from(n));
            } else {
                m.insert(k, Value::String(v.clone()));
            }
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

/// Who is acting: the agent from the environment, and the host half of a claim identity, which must
/// tell machines apart (never a shared fallback).
fn agent_and_host(rest: &[String]) -> anyhow::Result<(String, String)> {
    let agent = std::env::var("WEFTOS_AGENT").or_else(|_| std::env::var("USER")).unwrap_or_else(|_| "agent".into());
    let host = match local_host() {
        Some(h) => h,
        None if rest.iter().any(|a| a == "--host") => String::new(),
        None => anyhow::bail!("cannot tell this machine's host name; pass --host <name>"),
    };
    Ok((agent, host))
}

/// The environment variable a script sets once instead of passing `--as` on every `put`/`get`.
const AS_ENV: &str = "WEAVER_STORE_AS";

/// The principal a `put`/`get` is made as: `--as`, else `$WEAVER_STORE_AS`. It is only a name for the
/// request: the daemon gates it like any other (the principal needs a grant on the store), and every
/// request made as a principal is recorded on the chain as made by the operator (`store.as`).
fn request_principal(flag: Option<String>, env: Option<String>) -> anyhow::Result<String> {
    flag.or(env)
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("name the principal with --as <principal> (or set {AS_ENV}); it needs a grant on the store"))
}

/// Run the store subcommand.
pub async fn run(args: StoreArgs) -> anyhow::Result<()> {
    match args.action {
        StoreAction::Client(a) => return super::store_client_cmd::run(a).await,
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
        StoreAction::ArchiveGc { node_days, project_days, release_days, confirmed } => {
            let v = call("store.archive.gc", json!({"node_days": node_days, "project_days": project_days, "release_days": release_days, "confirmed": confirmed})).await?;
            let list = |k: &str| v[k].as_array().cloned().unwrap_or_default();
            for s in list("expired") {
                println!("{} {}  {} object(s), {} day(s) old", if confirmed { "deleted " } else { "would delete" }, cell(&s, "bucket"), cell(&s, "objects"), cell(&s, "age_days"));
            }
            for s in list("in_progress") {
                println!("kept (archive still being written): {}", cell(&s, "bucket"));
            }
            println!("{} set(s) kept, {} other key(s) left alone{}", cell(&v, "kept"), cell(&v, "not_ours"), if confirmed { "" } else { "; dry run, nothing deleted (--confirmed to delete)" });
        }
        StoreAction::ArchiveKeep { name, days, forever } => {
            let v = call("store.archive.keep", json!({"name": name, "days": days, "forever": forever})).await?;
            match (v["keep"]["days"].as_u64(), v["keep"].as_str()) {
                (Some(d), _) => println!("{name}: archives kept {d} day(s)"),
                (_, Some("forever")) => println!("{name}: archives kept for ever"),
                _ => println!("{name}: override cleared (its kind's limit applies)"),
            }
        }
        StoreAction::Put { name, key, text, principal } => {
            let principal = request_principal(principal, std::env::var(AS_ENV).ok())?;
            let v = call("store.put", json!({"name": name, "key": key, "text": text, "principal": principal})).await?;
            println!("wrote {} ({} bytes)", cell(&v, "key"), cell(&v, "size"));
        }
        StoreAction::Worktree { verb, rest } => {
            let (agent, host) = agent_and_host(&rest)?;
            let as_json = rest.iter().any(|a| a == "--json");
            let params = worktree_params(&rest, &agent, &host)?;
            let v = call(&format!("store.worktree.{verb}"), params).await?;
            match (as_json, wt_lines(&verb, &v)) {
                (false, Some(t)) => print!("{t}"),
                _ => println!("{}", serde_json::to_string_pretty(&v)?),
            }
        }
        StoreAction::Drive { verb, rest } => {
            let (agent, host) = agent_and_host(&rest)?;
            let params = worktree_params(&rest, &agent, &host)?;
            let v = call(&format!("store.drive.{verb}"), params).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        StoreAction::View { verb, rest } => {
            let (agent, host) = agent_and_host(&rest)?;
            let params = super::store_view_args::view_params(&rest, &agent, &host)?;
            let v = call(&format!("store.view.{verb}"), params).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        StoreAction::Release(a) => crate::commands::release_cmd::run(a).await?,
        StoreAction::Get { name, key, principal } => {
            let principal = request_principal(principal, std::env::var(AS_ENV).ok())?;
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
        assert!(p(&["put", "n", "k", "v", "--as", "cog:c"]).is_ok() && p(&["put", "n", "k", "v"]).is_ok(), "--as may come from the environment");
        assert!(p(&["create", "n", "--write", "a", "--write", "b"]).is_ok());
    }

    #[test]
    fn the_principal_comes_from_the_flag_then_the_environment_and_is_never_guessed() {
        let some = |s: &str| Some(s.to_owned());
        assert_eq!(request_principal(some("cog:a"), some("cog:b")).unwrap(), "cog:a", "the flag wins");
        assert_eq!(request_principal(None, some("cog:b")).unwrap(), "cog:b");
        assert!(request_principal(None, None).unwrap_err().to_string().contains("--as"));
        assert!(request_principal(None, some("  ")).is_err() && request_principal(some(""), None).is_err());
    }

    #[test]
    fn worktree_args_become_params() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let p = worktree_params(&a(&["--project", "01ABC", "--paths-z", "/t/p.z", "--force", "--json"]), "ag", "h1").unwrap();
        assert_eq!((p["project"].as_str(), p["paths_z"].as_str(), p["force"].as_bool(), p["agent"].as_str()), (Some("01ABC"), Some("/t/p.z"), Some(true), Some("ag")));
        let d = worktree_params(&a(&["--project", "01ABC", "--slug", "x", "--dry-run", "--on-conflict", "keep_project", "--max-bytes", "9"]), "ag", "h1").unwrap();
        assert_eq!((d["dry_run"].as_bool(), d["on_conflict"].as_str(), d["max_bytes"].as_u64()), (Some(true), Some("keep_project"), Some(9)));
        assert!(worktree_params(&a(&["--max-bytes", "lots"]), "a", "h").is_err());
        assert!(worktree_params(&a(&["--project"]), "a", "h").is_err(), "value required");
        assert!(worktree_params(&a(&["stray"]), "a", "h").is_err());
        assert_eq!(wt_lines("buckets", &json!({"worktree": "w-1", "project": "p-1"})).unwrap(), "w-1 p-1\n");
        assert!(StoreArgs::try_parse_from(["store", "worktree", "publish", "--project", "X", "--changes-z", "/f"]).is_ok());
        let d = worktree_params(&a(&["--name", "n", "--root", "/r", "--read-only"]), "ag", "h1").unwrap();
        assert_eq!((d["read_only"].as_bool(), d["root"].as_str()), (Some(true), Some("/r")));
        assert!(StoreArgs::try_parse_from(["store", "drive", "sync-up", "--name", "n", "--root", "/r"]).is_ok());
        assert!(StoreArgs::try_parse_from(["store", "view", "open", "--name", "n", "--root", "/r", "--include", "docs=docs", "--read-only"]).is_ok());
        assert!(StoreArgs::try_parse_from(["store", "view", "close", "--name", "n", "--root", "/r", "--discard"]).is_ok());
        assert!(StoreArgs::try_parse_from(["store", "view"]).is_err(), "a verb is required");
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
