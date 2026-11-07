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
