//! `weaver store client <verb>` (ADR-126 S5): the per-client-account store.
//!
//! Verbs map 1:1 onto the daemon's `store.client.*` RPCs (all Admin):
//! `status`, `hold <ulid>`, `grant|revoke` (local principals), `weftos-grant|weftos-revoke`
//! (WeftOS identities), `xmesh-trust|xmesh-install|xmesh-revoke` (foreign meshes), `sweep`,
//! and the governed `put|get|ls|delete`.
//!
//! `--as` is `local:<principal>` (e.g. `local:cog:c1`), `weftos:<id>`, or
//! `foreign:<64hex mesh>:<id>`. Anything else is refused before a request is sent.

use std::path::PathBuf;

use anyhow::{Context, bail};
use base64::Engine;
use clap::{Args, Subcommand};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::protocol;

/// Arguments of `weaver store client`.
#[derive(Args)]
pub struct StoreClientArgs {
    #[command(subcommand)]
    pub action: StoreClientAction,
}

/// `weaver store client` verbs.
#[derive(Subcommand)]
pub enum StoreClientAction {
    /// Gate state: held accounts, grants, trusted meshes.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Hold a client account (by ULID).
    Hold { ulid: String },
    /// Grant a local principal access to a client store.
    Grant { name: String, principal: String, access: String },
    /// Revoke a local principal's grant.
    Revoke { name: String, principal: String },
    /// Grant a WeftOS identity access.
    WeftosGrant { name: String, id: String, access: String },
    /// Revoke a WeftOS identity's grant.
    WeftosRevoke { name: String, id: String },
    /// Trust a foreign mesh's key (both 64 hex chars).
    XmeshTrust { mesh: String, key: String },
    /// Install a cross-mesh grant from a JSON file.
    XmeshInstall { grant: PathBuf },
    /// Revoke a cross-mesh grant from a JSON file.
    XmeshRevoke { grant: PathBuf },
    /// Sweep expired/revoked state.
    Sweep,
    /// Write an object as `--as`.
    Put {
        name: String,
        key: String,
        #[arg(long = "as")]
        as_: String,
        /// Text content.
        #[arg(long, conflicts_with = "file", required_unless_present = "file")]
        text: Option<String>,
        /// Read binary content from a file.
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Read an object as `--as`.
    Get {
        name: String,
        key: String,
        #[arg(long = "as")]
        as_: String,
        #[arg(long)]
        json: bool,
    },
    /// List objects as `--as`.
    Ls {
        name: String,
        #[arg(long)]
        prefix: Option<String>,
        #[arg(long = "as")]
        as_: String,
        #[arg(long)]
        json: bool,
    },
    /// Delete an object as `--as`.
    Delete {
        name: String,
        key: String,
        #[arg(long = "as")]
        as_: String,
        #[arg(long)]
        json: bool,
    },
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn hex_arg(what: &str, s: &str, len: usize) -> anyhow::Result<()> {
    if !is_hex(s, len) {
        bail!("{what} must be {len} hex characters");
    }
    Ok(())
}

fn access_arg(s: &str) -> anyhow::Result<()> {
    match s {
        "read" | "write" | "admin" => Ok(()),
        _ => bail!("access must be read, write or admin (got {s:?})"),
    }
}

/// Parse `--as` text into the `as` object. Refuses anything not in the three forms.
pub fn parse_as(s: &str) -> anyhow::Result<Value> {
    if let Some(p) = s.strip_prefix("local:") {
        if p.is_empty() || !p.contains(':') || p.ends_with(':') || p.starts_with(':') {
            bail!("--as local:<principal> needs a principal like cog:c1 (got {s:?})");
        }
        return Ok(json!({"kind": "local", "principal": p}));
    }
    if let Some(id) = s.strip_prefix("weftos:") {
        if id.is_empty() {
            bail!("--as weftos:<id> needs an id");
        }
        return Ok(json!({"kind": "weftos", "id": id}));
    }
    if let Some(rest) = s.strip_prefix("foreign:") {
        let Some((mesh, id)) = rest.split_once(':') else {
            bail!("--as foreign:<64hex mesh>:<id> (got {s:?})");
        };
        hex_arg("foreign mesh", mesh, 64)?;
        if id.is_empty() {
            bail!("--as foreign:<mesh>:<id> needs an id");
        }
        return Ok(json!({"kind": "foreign", "mesh": mesh, "id": id}));
    }
    bail!("--as must be local:<principal>, weftos:<id> or foreign:<64hex>:<id> (got {s:?})")
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantFile {
    owner: String,
    receiver: String,
    store: String,
    access: String,
    seq: u64,
    issued_at: u64,
    expires_at: u64,
    sig: String,
}

/// Parse and validate a cross-mesh grant JSON document into the RPC grant object.
pub fn grant_from_str(text: &str) -> anyhow::Result<Value> {
    let g: GrantFile = serde_json::from_str(text).context("grant file is not a valid grant object")?;
    hex_arg("owner", &g.owner, 64)?;
    hex_arg("receiver", &g.receiver, 64)?;
    hex_arg("sig", &g.sig, 128)?;
    access_arg(&g.access)?;
    Ok(json!({"owner": g.owner, "receiver": g.receiver, "store": g.store, "access": g.access,
        "seq": g.seq, "issued_at": g.issued_at, "expires_at": g.expires_at, "sig": g.sig}))
}

fn grant_from_file(p: &PathBuf) -> anyhow::Result<Value> {
    let text = std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
    grant_from_str(&text)
}

/// Build the RPC method and params for a verb, validating client-side.
pub fn build(action: &StoreClientAction) -> anyhow::Result<(&'static str, Value)> {
    use StoreClientAction as A;
    Ok(match action {
        A::Status { .. } => ("store.client.status", json!({})),
        A::Hold { ulid } => ("store.client.hold", json!({"ulid": ulid})),
        A::Grant { name, principal, access } => {
            access_arg(access)?;
            ("store.client.grant", json!({"name": name, "principal": principal, "access": access}))
        }
        A::Revoke { name, principal } => ("store.client.revoke", json!({"name": name, "principal": principal})),
        A::WeftosGrant { name, id, access } => {
            access_arg(access)?;
            ("store.client.weftos_grant", json!({"name": name, "id": id, "access": access}))
        }
        A::WeftosRevoke { name, id } => ("store.client.weftos_revoke", json!({"name": name, "id": id})),
        A::XmeshTrust { mesh, key } => {
            hex_arg("mesh", mesh, 64)?;
            hex_arg("key", key, 64)?;
            ("store.client.xmesh_trust", json!({"mesh": mesh, "key": key}))
        }
        A::XmeshInstall { grant } => ("store.client.xmesh_install", json!({"grant": grant_from_file(grant)?})),
        A::XmeshRevoke { grant } => ("store.client.xmesh_revoke", json!({"grant": grant_from_file(grant)?})),
        A::Sweep => ("store.client.sweep", json!({})),
        A::Put { name, key, as_, text, file, .. } => {
            let as_v = parse_as(as_)?;
            let mut p = json!({"name": name, "key": key, "as": as_v});
            match (text, file) {
                (Some(t), None) => p["text"] = json!(t),
                (None, Some(f)) => {
                    let bytes = std::fs::read(f).with_context(|| format!("reading {}", f.display()))?;
                    p["b64"] = json!(base64::engine::general_purpose::STANDARD.encode(bytes));
                }
                _ => bail!("put needs exactly one of --text or --file"),
            }
            ("store.client.put", p)
        }
        A::Get { name, key, as_, .. } => ("store.client.get", json!({"name": name, "key": key, "as": parse_as(as_)?})),
        A::Ls { name, prefix, as_, .. } => ("store.client.ls", json!({"name": name, "prefix": prefix, "as": parse_as(as_)?})),
        A::Delete { name, key, as_, .. } => ("store.client.delete", json!({"name": name, "key": key, "as": parse_as(as_)?})),
    })
}

fn wants_json(a: &StoreClientAction) -> bool {
    use StoreClientAction as A;
    matches!(a, A::Status { json: true } | A::Put { json: true, .. } | A::Get { json: true, .. }
        | A::Ls { json: true, .. } | A::Delete { json: true, .. })
}

fn cell(v: &Value, k: &str) -> String {
    match &v[k] {
        Value::Null => "-".into(),
        Value::String(s) => s.clone(),
        o => o.to_string(),
    }
}

/// Human rendering of a verb's result.
pub fn render(action: &StoreClientAction, v: &Value) -> anyhow::Result<String> {
    use StoreClientAction as A;
    Ok(match action {
        A::Status { .. } => serde_json::to_string_pretty(v)? + "\n",
        A::Hold { ulid } => format!("held client account {ulid}\n"),
        A::Grant { name, principal, access } => format!("{principal} on {name}: {access}\n"),
        A::Revoke { name, principal } => format!("revoked {principal} on {name}\n"),
        A::WeftosGrant { name, id, access } => format!("weftos:{id} on {name}: {access}\n"),
        A::WeftosRevoke { name, id } => format!("revoked weftos:{id} on {name}\n"),
        A::XmeshTrust { mesh, .. } => format!("trusted mesh {mesh}\n"),
        A::XmeshInstall { .. } => "installed cross-mesh grant\n".into(),
        A::XmeshRevoke { .. } => "revoked cross-mesh grant\n".into(),
        A::Sweep => format!("swept: {}\n", serde_json::to_string(v)?),
        A::Put { .. } => format!("wrote {} ({} bytes)\n", cell(v, "key"), cell(v, "size")),
        A::Get { .. } => match v["text"].as_str() {
            Some(t) => t.to_string(),
            None => bail!("{} bytes of binary data; use --json", cell(v, "size")),
        },
        A::Ls { .. } => {
            let objs = v["objects"].as_array().cloned().unwrap_or_default();
            if objs.is_empty() {
                "no objects\n".into()
            } else {
                objs.iter().map(|o| format!("{:>10}  {}\n", cell(o, "size"), cell(o, "key"))).collect()
            }
        }
        A::Delete { key, .. } => format!("deleted {key}\n"),
    })
}

async fn call(method: &str, params: Value) -> anyhow::Result<Value> {
    let mut client = clawft_rpc::connect_or_bail().await?;
    let resp = client.call(protocol::Request::with_params(method, params)).await?;
    if !resp.ok {
        anyhow::bail!(resp.error.unwrap_or_else(|| "unknown error".into()));
    }
    Ok(resp.result.unwrap_or_default())
}

/// Run `weaver store client`.
pub async fn run(args: StoreClientArgs) -> anyhow::Result<()> {
    let (method, params) = build(&args.action)?;
    let v = call(method, params).await?;
    if wants_json(&args.action) {
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        print!("{}", render(&args.action, &v)?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const H64: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const H64B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn parse(a: &[&str]) -> Result<StoreClientAction, clap::Error> {
        use clap::Parser;
        #[derive(Parser)]
        struct W {
            #[command(flatten)]
            c: StoreClientArgs,
        }
        W::try_parse_from(std::iter::once("client").chain(a.iter().copied())).map(|w| w.c.action)
    }

    fn built(a: &[&str]) -> (&'static str, Value) {
        build(&parse(a).unwrap()).unwrap()
    }

    #[test]
    fn verbs_parse_and_required_args_are_enforced() {
        assert!(parse(&["status", "--json"]).is_ok() && parse(&["sweep"]).is_ok());
        assert!(parse(&["hold", "01ABC"]).is_ok() && parse(&["hold"]).is_err());
        assert!(parse(&["grant", "n", "cog:c1", "read"]).is_ok() && parse(&["grant", "n", "cog:c1"]).is_err());
        assert!(parse(&["weftos-grant", "n", "weaver", "write"]).is_ok());
        assert!(parse(&["xmesh-trust", H64, H64B]).is_ok() && parse(&["xmesh-trust", H64]).is_err());
        assert!(parse(&["put", "n", "k", "--as", "weftos:w", "--text", "t"]).is_ok());
        assert!(parse(&["put", "n", "k", "--as", "weftos:w"]).is_err(), "needs --text or --file");
        assert!(parse(&["put", "n", "k", "--as", "weftos:w", "--text", "t", "--file", "f"]).is_err());
        assert!(parse(&["put", "n", "k", "--text", "t"]).is_err(), "needs --as");
        assert!(parse(&["get", "n", "k"]).is_err() && parse(&["ls", "n", "--as", "weftos:w", "--prefix", "p/"]).is_ok());
    }

    #[test]
    fn as_parsing_good_and_bad() {
        assert_eq!(parse_as("local:cog:c1").unwrap(), json!({"kind": "local", "principal": "cog:c1"}));
        assert_eq!(parse_as("weftos:weaver").unwrap(), json!({"kind": "weftos", "id": "weaver"}));
        assert_eq!(
            parse_as(&format!("foreign:{H64}:x")).unwrap(),
            json!({"kind": "foreign", "mesh": H64, "id": "x"})
        );
        for bad in ["", "cog:c1", "local:", "local:c1", "local:cog:", "weftos:", "foreign:x", "foreign:abc:x",
            &format!("foreign:{H64}:"), &format!("foreign:{H64}"), "other:a:b"] {
            assert!(parse_as(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn params_for_each_verb() {
        assert_eq!(built(&["status"]), ("store.client.status", json!({})));
        assert_eq!(built(&["sweep"]), ("store.client.sweep", json!({})));
        assert_eq!(built(&["hold", "U1"]), ("store.client.hold", json!({"ulid": "U1"})));
        assert_eq!(built(&["grant", "n", "cog:c1", "write"]),
            ("store.client.grant", json!({"name": "n", "principal": "cog:c1", "access": "write"})));
        assert_eq!(built(&["revoke", "n", "cog:c1"]),
            ("store.client.revoke", json!({"name": "n", "principal": "cog:c1"})));
        assert_eq!(built(&["weftos-grant", "n", "weaver", "admin"]),
            ("store.client.weftos_grant", json!({"name": "n", "id": "weaver", "access": "admin"})));
        assert_eq!(built(&["weftos-revoke", "n", "weaver"]),
            ("store.client.weftos_revoke", json!({"name": "n", "id": "weaver"})));
        assert_eq!(built(&["xmesh-trust", H64, H64B]),
            ("store.client.xmesh_trust", json!({"mesh": H64, "key": H64B})));
        assert_eq!(built(&["put", "n", "k", "--as", "local:cog:c1", "--text", "hi"]),
            ("store.client.put", json!({"name": "n", "key": "k", "as": {"kind": "local", "principal": "cog:c1"}, "text": "hi"})));
        assert_eq!(built(&["get", "n", "k", "--as", "weftos:w"]),
            ("store.client.get", json!({"name": "n", "key": "k", "as": {"kind": "weftos", "id": "w"}})));
        assert_eq!(built(&["ls", "n", "--prefix", "p/", "--as", "weftos:w"]),
            ("store.client.ls", json!({"name": "n", "prefix": "p/", "as": {"kind": "weftos", "id": "w"}})));
        assert_eq!(built(&["ls", "n", "--as", "weftos:w"]).1["prefix"], Value::Null);
        assert_eq!(built(&["delete", "n", "k", "--as", "weftos:w"]),
            ("store.client.delete", json!({"name": "n", "key": "k", "as": {"kind": "weftos", "id": "w"}})));
    }

    #[test]
    fn put_file_is_base64_and_missing_file_refuses() {
        let dir = std::env::temp_dir().join(format!("store_client_cmd_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("blob.bin");
        std::fs::write(&f, [0u8, 1, 2, 255]).unwrap();
        let (m, p) = built(&["put", "n", "k", "--as", "weftos:w", "--file", f.to_str().unwrap()]);
        assert_eq!(m, "store.client.put");
        assert_eq!(p["b64"], "AAEC/w==");
        assert!(p.get("text").is_none());
        std::fs::remove_dir_all(&dir).ok();
        assert!(build(&parse(&["put", "n", "k", "--as", "weftos:w", "--file", "/nonexistent/x"]).unwrap()).is_err());
    }

    #[test]
    fn bad_hex_and_access_are_refused_client_side() {
        let b = |a: &[&str]| build(&parse(a).unwrap());
        assert!(b(&["xmesh-trust", "abc", H64]).is_err() && b(&["xmesh-trust", H64, "abc"]).is_err());
        let nonhex = "z".repeat(64);
        assert!(b(&["xmesh-trust", &nonhex, H64]).is_err());
        assert!(b(&["grant", "n", "cog:c1", "owner"]).is_err());
        assert!(b(&["weftos-grant", "n", "w", "root"]).is_err());
        assert!(b(&["get", "n", "k", "--as", "bogus"]).is_err());
    }

    fn grant_json(sig: &str, access: &str) -> String {
        format!(r#"{{"owner":"{H64}","receiver":"{H64B}","store":"s","access":"{access}","seq":1,"issued_at":10,"expires_at":20,"sig":"{sig}"}}"#)
    }

    #[test]
    fn grant_file_validation() {
        let sig = "c".repeat(128);
        let g = grant_from_str(&grant_json(&sig, "read")).unwrap();
        assert_eq!(g, json!({"owner": H64, "receiver": H64B, "store": "s", "access": "read",
            "seq": 1, "issued_at": 10, "expires_at": 20, "sig": sig}));
        assert!(grant_from_str(&grant_json(&"c".repeat(127), "read")).is_err(), "short sig");
        assert!(grant_from_str(&grant_json(&sig, "root")).is_err(), "bad access");
        assert!(grant_from_str("{}").is_err() && grant_from_str("not json").is_err());
        let neg = grant_json(&sig, "read").replace("\"seq\":1", "\"seq\":-1");
        assert!(grant_from_str(&neg).is_err(), "seq is u64");
        let extra = grant_json(&sig, "read").replace("\"seq\":1", "\"seq\":1,\"x\":2");
        assert!(grant_from_str(&extra).is_err(), "unknown fields refused");
        let short_owner = grant_json(&sig, "read").replace(H64, "abcd");
        assert!(grant_from_str(&short_owner).is_err());
    }

    #[test]
    fn render_results() {
        let a = parse(&["ls", "n", "--as", "weftos:w"]).unwrap();
        assert!(render(&a, &json!({"objects": []})).unwrap().contains("no objects"));
        let t = render(&a, &json!({"objects": [{"key": "k1", "size": 3}]})).unwrap();
        assert!(t.contains("k1") && t.contains('3'), "{t}");
        let g = parse(&["get", "n", "k", "--as", "weftos:w"]).unwrap();
        assert_eq!(render(&g, &json!({"text": "hello"})).unwrap(), "hello");
        assert!(render(&g, &json!({"size": 4})).is_err(), "binary refuses");
    }
}
