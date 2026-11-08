//! `weaver store client <verb>` (ADR-126 S5): the per-client-account store.
//!
//! Verbs map 1:1 onto the daemon's `store.client.*` RPCs (all Admin):
//! `status`, `hold <ulid>`, `grant|revoke` (local principals), `weftos-grant|weftos-revoke`
//! (WeftOS identities), `xmesh-trust|xmesh-install|xmesh-revoke` (foreign meshes), `sweep`,
//! `snapshot` (publish a signed gate snapshot on the chain), `archive` (seal the chain's events into
//! a range file), `archive-verify <FILE>` (re-check an archived range file), and the governed
//! `put|get|ls|delete`.
//!
//! `--as` is `local:<principal>` (e.g. `local:cog:c1`), `weftos:<id>`, or
//! `foreign:<64hex mesh>:<id>`. Anything else is refused before a request is sent.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use base64::Engine;
use clap::{Args, Subcommand};
use serde::Deserialize;
use serde_json::{Value, json};

use ed25519_dalek::SigningKey;

use crate::protocol;
use crate::store_account::{Access, CrossMeshGrant, MAX_GRANT_TTL_SECS};
use crate::weftos_uri::{Kind, WeftosUri};

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
    XmeshInstall {
        grant: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Revoke a cross-mesh grant from a JSON file.
    XmeshRevoke {
        grant: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Sign a cross-mesh grant locally (no daemon call) as the owning mesh.
    XmeshSign(XmeshSignArgs),
    /// Sweep expired/revoked state.
    Sweep {
        #[arg(long)]
        json: bool,
    },
    /// Publish a signed snapshot of the gate on the chain.
    Snapshot {
        #[arg(long)]
        json: bool,
    },
    /// Archive the chain's events into a sealed range file.
    Archive {
        #[arg(long)]
        json: bool,
    },
    /// Verify an archived range file by name.
    ArchiveVerify {
        /// Bare file name of an archive (no path separators, not dot-prefixed).
        file: String,
        #[arg(long)]
        json: bool,
    },
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

/// Arguments of `weaver store client xmesh-sign`.
#[derive(Args, Debug)]
pub struct XmeshSignArgs {
    /// File holding the owning mesh's ed25519 signing seed (64 hex chars; mode 0600).
    #[arg(long)]
    pub key_file: PathBuf,
    /// Owning mesh id (64 hex).
    #[arg(long)]
    pub owner: String,
    /// Receiving mesh id (64 hex).
    #[arg(long)]
    pub receiver: String,
    /// Store name, `weftos://<owner>/projects/<ULID>/store`.
    #[arg(long)]
    pub store: String,
    /// read, write or admin.
    #[arg(long)]
    pub access: String,
    /// Monotonic grant sequence number.
    #[arg(long)]
    pub seq: u64,
    /// Lifetime in seconds (1..=MAX_GRANT_TTL_SECS).
    #[arg(long)]
    pub ttl_secs: u64,
    /// Override the issue time (unix seconds); for tests.
    #[arg(long)]
    pub now: Option<u64>,
    /// Write the grant here instead of stdout.
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Overwrite `--out` if it exists.
    #[arg(long)]
    pub force: bool,
}

fn hex32(what: &str, s: &str) -> Result<[u8; 32], String> {
    if !is_hex(s, 64) {
        return Err(format!("{what} must be 64 hex characters"));
    }
    let mut b = [0u8; 32];
    hex::decode_to_slice(s, &mut b).map_err(|e| format!("{what}: {e}"))?;
    Ok(b)
}

/// Read a signing seed file: exactly 64 hex chars (whitespace trimmed). On unix a file that
/// group or others can access is refused. The key is never included in an error.
pub fn read_key_file(p: &Path) -> Result<SigningKey, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let md = std::fs::metadata(p).map_err(|e| format!("key file {}: {e}", p.display()))?;
        if md.permissions().mode() & 0o077 != 0 {
            return Err(format!(
                "key file {} is accessible by group or others (mode {:o}); run chmod 600 on it",
                p.display(),
                md.permissions().mode() & 0o777
            ));
        }
    }
    let text = std::fs::read_to_string(p).map_err(|e| format!("key file {}: {e}", p.display()))?;
    let seed = hex32("key file contents", text.trim()).map_err(|_| {
        format!("key file {} must hold exactly 64 hex characters (a 32-byte seed)", p.display())
    })?;
    Ok(SigningKey::from_bytes(&seed))
}

fn access_of(s: &str) -> Result<Access, String> {
    match s {
        "read" => Ok(Access::Read),
        "write" => Ok(Access::Write),
        "admin" => Ok(Access::Admin),
        _ => Err(format!("access must be read, write or admin (got {s:?})")),
    }
}

/// Sign a cross-mesh grant as the owning mesh. Pure: no clock, no I/O.
#[allow(clippy::too_many_arguments)]
pub fn sign_grant(
    key: &SigningKey, owner: &str, receiver: &str, store: &str, access: &str, seq: u64, ttl_secs: u64, now: u64,
) -> Result<CrossMeshGrant, String> {
    let owner_b = hex32("owner", owner)?;
    let receiver_b = hex32("receiver", receiver)?;
    let access = access_of(access)?;
    if ttl_secs == 0 || ttl_secs > MAX_GRANT_TTL_SECS {
        return Err(format!("ttl-secs must be 1..={MAX_GRANT_TTL_SECS} (got {ttl_secs})"));
    }
    let uri = WeftosUri::parse(store).map_err(|e| format!("store {store:?}: {e}"))?;
    if uri.kind != Kind::Projects {
        return Err(format!("store {store:?} must name a projects store"));
    }
    if uri.authority.0 != owner_b {
        return Err(format!("store {store:?} is not owned by mesh {owner}"));
    }
    let expires = now.checked_add(ttl_secs).ok_or("now + ttl overflows")?;
    Ok(CrossMeshGrant::sign(key, owner_b, receiver_b, store, access, seq, now, expires))
}

fn now_secs() -> anyhow::Result<u64> {
    Ok(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs())
}

/// Write `text` to `p` with mode 0600 (unix); refuses an existing file unless `force`.
fn write_private(p: &Path, text: &str, force: bool) -> anyhow::Result<()> {
    use std::io::Write;
    let mut o = std::fs::OpenOptions::new();
    o.write(true);
    if force {
        o.create(true).truncate(true);
    } else {
        o.create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let mut f = o.open(p).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            anyhow::anyhow!("{} exists; pass --force to overwrite", p.display())
        } else {
            anyhow::anyhow!("writing {}: {e}", p.display())
        }
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    f.write_all(text.as_bytes())?;
    Ok(())
}

/// Run `xmesh-sign`: returns the text to print (empty when written to `--out`, with a note).
pub fn run_xmesh_sign(a: &XmeshSignArgs) -> anyhow::Result<String> {
    let key = read_key_file(&a.key_file).map_err(|e| anyhow::anyhow!(e))?;
    let now = match a.now {
        Some(n) => n,
        None => now_secs()?,
    };
    let g = sign_grant(&key, &a.owner, &a.receiver, &a.store, &a.access, a.seq, a.ttl_secs, now)
        .map_err(|e| anyhow::anyhow!(e))?;
    let text = serde_json::to_string_pretty(&g.to_json())? + "\n";
    match &a.out {
        Some(p) => {
            write_private(p, &text, a.force)?;
            Ok(format!("wrote cross-mesh grant to {} (expires {})\n", p.display(), g.expires_at))
        }
        None => Ok(text),
    }
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

/// Client-side check of an archive file name: a bare name, as the daemon requires.
pub fn archive_name(s: &str) -> anyhow::Result<()> {
    if s.is_empty() {
        bail!("archive file name must not be empty");
    }
    if s.contains('/') || s.contains('\\') {
        bail!("archive file name must not contain '/' or '\\' (got {s:?})");
    }
    if s.starts_with('.') {
        bail!("archive file name must not start with '.' (got {s:?})");
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
        A::XmeshInstall { grant, .. } => ("store.client.xmesh_install", json!({"grant": grant_from_file(grant)?})),
        A::XmeshRevoke { grant, .. } => ("store.client.xmesh_revoke", json!({"grant": grant_from_file(grant)?})),
        A::XmeshSign(_) => bail!("xmesh-sign is local and sends no request"),
        A::Sweep { .. } => ("store.client.sweep", json!({})),
        A::Snapshot { .. } => ("store.client.snapshot", json!({})),
        A::Archive { .. } => ("store.client.archive", json!({})),
        A::ArchiveVerify { file, .. } => {
            archive_name(file)?;
            ("store.client.archive_verify", json!({"file": file}))
        }
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
    matches!(a, A::Status { json: true } | A::Sweep { json: true } | A::Snapshot { json: true }
        | A::Archive { json: true } | A::ArchiveVerify { json: true, .. }
        | A::XmeshInstall { json: true, .. }
        | A::XmeshRevoke { json: true, .. } | A::Put { json: true, .. } | A::Get { json: true, .. }
        | A::Ls { json: true, .. } | A::Delete { json: true, .. })
}

fn cell(v: &Value, k: &str) -> String {
    match &v[k] {
        Value::Null => "-".into(),
        Value::String(s) => s.clone(),
        o => o.to_string(),
    }
}

fn grant_line(verb: &str, g: &Value) -> String {
    let recv = cell(g, "receiver");
    format!(
        "{verb} cross-mesh grant: {} to {} access={} seq={} expires={}\n",
        cell(g, "store"), recv.get(..12).unwrap_or(&recv), cell(g, "access"), cell(g, "seq"), cell(g, "expires_at")
    )
}

/// Human rendering of a verb's result.
pub fn render(action: &StoreClientAction, v: &Value) -> anyhow::Result<String> {
    use StoreClientAction as A;
    Ok(match action {
        A::Status { .. } => format!(
            "account  {}\nuid      {}\nroot     {}\nlisten   {}\n",
            cell(v, "account"), cell(v, "uid"), cell(v, "root"), cell(v, "listen")
        ),
        A::Hold { ulid } => format!("held client account {ulid}\n"),
        A::Grant { name, principal, access } => format!("{principal} on {name}: {access}\n"),
        A::Revoke { name, principal } => format!("revoked {principal} on {name}\n"),
        A::WeftosGrant { name, id, access } => format!("weftos:{id} on {name}: {access}\n"),
        A::WeftosRevoke { name, id } => format!("revoked weftos:{id} on {name}\n"),
        A::XmeshTrust { mesh, .. } => format!("trusted mesh {mesh}\n"),
        A::XmeshInstall { .. } => grant_line("installed", &v["installed"]),
        A::XmeshRevoke { .. } => grant_line("revoked", &v["revoked"]),
        A::XmeshSign(_) => bail!("xmesh-sign has no daemon result"),
        A::Sweep { .. } => match v["expired"].as_u64() {
            Some(0) => "no expired grants\n".into(),
            Some(n) => format!("expired grants dropped: {n}\n"),
            None => bail!("sweep result has no expired count"),
        },
        A::Snapshot { .. } => match v["through_seq"].as_u64() {
            Some(n) => format!("gate snapshot published, covers chain event {n}\n"),
            None => bail!("snapshot result has no through_seq"),
        },
        A::Archive { .. } => {
            let Some(file) = v["file"].as_str() else { bail!("archive result has no file") };
            let Some(through) = v["through_seq"].as_u64() else { bail!("archive result has no through_seq") };
            let Some(count) = v["event_count"].as_u64() else { bail!("archive result has no event_count") };
            let head = cell(v, "events_head");
            format!(
                "archived  {file}\npath      {}\ncovers    {count} events through chain event {through}\nhead      {}\n",
                cell(v, "path"), head.get(..12).unwrap_or(&head)
            )
        }
        A::ArchiveVerify { file, .. } => {
            if v["verified"].as_bool() != Some(true) {
                bail!("archive verify result is not verified");
            }
            let Some(through) = v["through_seq"].as_u64() else { bail!("archive verify result has no through_seq") };
            let Some(count) = v["event_count"].as_u64() else { bail!("archive verify result has no event_count") };
            format!("verified  {file}\ncovers    {count} events through chain event {through}\n")
        }
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
    if let StoreClientAction::XmeshSign(a) = &args.action {
        print!("{}", run_xmesh_sign(a)?);
        return Ok(());
    }
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
        assert!(parse(&["snapshot"]).is_ok() && parse(&["snapshot", "--json"]).is_ok());
        assert!(parse(&["snapshot", "extra"]).is_err(), "snapshot takes no positional args");
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
        assert_eq!(built(&["snapshot"]), ("store.client.snapshot", json!({})));
        assert_eq!(built(&["snapshot", "--json"]), ("store.client.snapshot", json!({})));
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

    // ---- xmesh-sign ----

    const ULID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    const NOW: u64 = 1_800_000_000;

    fn skey() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn store_of(owner: &str) -> String {
        format!("weftos://{owner}/projects/{ULID}/store")
    }

    #[test]
    fn sign_grant_round_trips_through_the_gate() {
        use crate::store_account::StoreGate;
        use crate::weftos_uri::Authority;
        let (owner, recv) = (H64, H64B);
        let g = sign_grant(&skey(), owner, recv, &store_of(owner), "write", 3, 600, NOW).unwrap();
        assert_eq!((g.issued_at, g.expires_at, g.seq, g.access), (NOW, NOW + 600, 3, Access::Write));
        // Survives the file form that xmesh-install reads.
        let back = grant_from_str(&serde_json::to_string(&g.to_json()).unwrap()).unwrap();
        assert_eq!(CrossMeshGrant::from_json(&back).unwrap(), g);
        let mut gate = StoreGate::new(Authority(hex32("r", recv).unwrap()), "acme");
        assert!(gate.install_xmesh(g.clone(), NOW).is_err(), "owner not trusted yet");
        gate.trust_mesh(hex32("o", owner).unwrap(), skey().verifying_key());
        assert!(gate.install_xmesh(g.clone(), NOW).is_ok());
        // A different key's signature is refused.
        let mut gate2 = StoreGate::new(Authority(hex32("r", recv).unwrap()), "acme");
        gate2.trust_mesh(hex32("o", owner).unwrap(), SigningKey::from_bytes(&[8u8; 32]).verifying_key());
        assert!(gate2.install_xmesh(g, NOW).is_err());
    }

    #[test]
    fn sign_grant_refusals() {
        let ok = |store: &str, access: &str, ttl: u64| sign_grant(&skey(), H64, H64B, store, access, 1, ttl, NOW);
        let name = store_of(H64);
        assert!(ok(&name, "read", 1).is_ok() && ok(&name, "admin", MAX_GRANT_TTL_SECS).is_ok());
        assert!(ok(&store_of(H64B), "read", 10).unwrap_err().contains("not owned"), "wrong owner");
        assert!(ok(&name, "read", 0).is_err(), "ttl 0");
        assert!(ok(&name, "read", MAX_GRANT_TTL_SECS + 1).is_err(), "ttl too long");
        assert!(ok(&name, "root", 10).is_err(), "bad access");
        assert!(ok("not a uri", "read", 10).is_err());
        assert!(ok(&format!("weftos://{H64}/goals/{ULID}"), "read", 10).is_err(), "not a projects store");
        assert!(sign_grant(&skey(), "abc", H64B, &name, "read", 1, 10, NOW).is_err(), "short owner");
        assert!(sign_grant(&skey(), H64, "zz", &name, "read", 1, 10, NOW).is_err(), "bad receiver");
        assert!(sign_grant(&skey(), H64, H64B, &name, "read", 1, 10, u64::MAX).is_err(), "overflow");
    }

    fn key_file(dir: &Path, name: &str, body: &str, mode: u32) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        let _ = mode;
        p
    }

    #[test]
    fn key_file_validation() {
        let d = tempfile::tempdir().unwrap();
        let seed = "07".repeat(32);
        let good = key_file(d.path(), "good", &format!("  {seed}\n"), 0o600);
        assert_eq!(read_key_file(&good).unwrap().to_bytes(), [7u8; 32], "whitespace trimmed");
        for (n, body) in [("short", "07".repeat(31)), ("nonhex", "zz".repeat(32)), ("long", "07".repeat(33)), ("empty", String::new())] {
            let e = read_key_file(&key_file(d.path(), n, &body, 0o600)).unwrap_err();
            assert!(e.contains("64 hex"), "{n}: {e}");
            assert!(!e.contains(&body) || body.is_empty(), "key material must not be echoed");
        }
        assert!(read_key_file(&d.path().join("missing")).is_err());
        #[cfg(unix)]
        for mode in [0o640, 0o604, 0o644, 0o660] {
            let e = read_key_file(&key_file(d.path(), "loose", &seed, mode)).unwrap_err();
            assert!(e.contains("group or others") && !e.contains(&seed), "{mode:o}: {e}");
        }
    }

    fn sign_args(d: &Path, extra: &[&str]) -> XmeshSignArgs {
        let kf = key_file(d, "k", &"07".repeat(32), 0o600);
        let (store, kfs) = (store_of(H64), kf.to_str().unwrap().to_string());
        let mut a = vec!["xmesh-sign", "--key-file", &kfs, "--owner", H64, "--receiver", H64B, "--store", &store,
            "--access", "read", "--seq", "5", "--ttl-secs", "100", "--now", "1800000000"];
        a.extend_from_slice(extra);
        match parse(&a).unwrap() {
            StoreClientAction::XmeshSign(x) => x,
            _ => panic!("not xmesh-sign"),
        }
    }

    #[test]
    fn xmesh_sign_args_parse() {
        let d = tempfile::tempdir().unwrap();
        let a = sign_args(d.path(), &[]);
        assert_eq!((a.seq, a.ttl_secs, a.now, a.force), (5, 100, Some(NOW), false));
        assert!(a.out.is_none());
        let a = sign_args(d.path(), &["--out", "g.json", "--force"]);
        assert!(a.force && a.out.is_some());
        assert!(parse(&["xmesh-sign"]).is_err(), "required args");
        assert!(parse(&["xmesh-sign", "--key-file", "k", "--owner", H64, "--receiver", H64B, "--store", "s",
            "--access", "read", "--ttl-secs", "5"]).is_err(), "--seq required");
        assert!(parse(&["xmesh-sign", "--key-file", "k", "--owner", H64, "--receiver", H64B, "--store", "s",
            "--access", "read", "--seq", "1"]).is_err(), "--ttl-secs required");
        assert!(build(&StoreClientAction::XmeshSign(a)).is_err(), "never sent to the daemon");
    }

    #[test]
    fn xmesh_sign_stdout_and_out_file() {
        let d = tempfile::tempdir().unwrap();
        let text = run_xmesh_sign(&sign_args(d.path(), &[])).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!((v["seq"].clone(), v["expires_at"].clone()), (json!(5), json!(NOW + 100)));
        assert!(grant_from_str(&text).is_ok(), "xmesh-install can read it");

        let out = d.path().join("grant.json");
        let o = out.to_str().unwrap();
        run_xmesh_sign(&sign_args(d.path(), &["--out", o])).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&out).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(std::fs::read_to_string(&out).unwrap(), text);
        std::fs::write(&out, "precious").unwrap();
        let e = run_xmesh_sign(&sign_args(d.path(), &["--out", o])).unwrap_err().to_string();
        assert!(e.contains("--force"), "{e}");
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "precious", "not overwritten");
        run_xmesh_sign(&sign_args(d.path(), &["--out", o, "--force"])).unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap(), text);
    }

    #[test]
    fn human_rendering_of_status_sweep_and_xmesh() {
        let st = parse(&["status"]).unwrap();
        let v = json!({"account": "acme", "uid": 2102, "root": "/data1/acme/store", "listen": "127.0.0.1:9100"});
        assert_eq!(render(&st, &v).unwrap(),
            "account  acme\nuid      2102\nroot     /data1/acme/store\nlisten   127.0.0.1:9100\n");
        let sw = parse(&["sweep"]).unwrap();
        assert_eq!(render(&sw, &json!({"expired": 0})).unwrap(), "no expired grants\n");
        assert_eq!(render(&sw, &json!({"expired": 3})).unwrap(), "expired grants dropped: 3\n");
        assert!(render(&sw, &json!({})).is_err(), "malformed result refuses");
        let g = sign_grant(&skey(), H64, H64B, &store_of(H64), "read", 9, 60, NOW).unwrap().to_json();
        let ins = parse(&["xmesh-install", "g.json"]).unwrap();
        let t = render(&ins, &json!({"installed": g})).unwrap();
        assert!(t.starts_with("installed") && t.contains(&store_of(H64)) && t.contains("bbbbbbbbbbbb")
            && !t.contains(H64B) && t.contains("access=read") && t.contains("seq=9")
            && t.contains(&format!("expires={}", NOW + 60)), "{t}");
        let rev = parse(&["xmesh-revoke", "g.json"]).unwrap();
        assert!(render(&rev, &json!({"revoked": g})).unwrap().starts_with("revoked"));
        assert!(wants_json(&parse(&["sweep", "--json"]).unwrap()) && !wants_json(&sw));
        assert!(wants_json(&parse(&["xmesh-install", "g", "--json"]).unwrap()));
        assert!(wants_json(&parse(&["xmesh-revoke", "g", "--json"]).unwrap()));
    }

    #[test]
    fn snapshot_human_rendering_and_refusal() {
        let snap = parse(&["snapshot"]).unwrap();
        assert_eq!(render(&snap, &json!({"through_seq": 42})).unwrap(),
            "gate snapshot published, covers chain event 42\n");
        assert_eq!(render(&snap, &json!({"through_seq": 0})).unwrap(),
            "gate snapshot published, covers chain event 0\n");
        assert!(render(&snap, &json!({})).is_err(), "missing through_seq refuses");
        assert!(render(&snap, &json!({"through_seq": "42"})).is_err(), "non-integer through_seq refuses");
        assert!(render(&snap, &json!({"expired": 3})).is_err(), "sweep's field is not a snapshot result");
    }

    #[test]
    fn snapshot_json_flag_selects_raw_output() {
        let human = parse(&["snapshot"]).unwrap();
        let raw = parse(&["snapshot", "--json"]).unwrap();
        assert!(!wants_json(&human) && wants_json(&raw));
        assert!(matches!(raw, StoreClientAction::Snapshot { json: true }));
    }

    // ---- archive / archive-verify ----

    #[test]
    fn archive_verbs_parse_and_required_args_are_enforced() {
        assert!(parse(&["archive"]).is_ok() && parse(&["archive", "--json"]).is_ok());
        assert!(parse(&["archive", "extra"]).is_err(), "archive takes no positional args");
        assert!(parse(&["archive-verify", "2026-11.archive"]).is_ok());
        assert!(parse(&["archive-verify", "2026-11.archive", "--json"]).is_ok());
        assert!(parse(&["archive-verify"]).is_err(), "archive-verify needs a file");
        assert!(parse(&["archive-verify", "a", "b"]).is_err(), "one file only");
    }

    #[test]
    fn archive_params_and_verify_params_are_exact() {
        assert_eq!(built(&["archive"]), ("store.client.archive", json!({})));
        assert_eq!(built(&["archive", "--json"]), ("store.client.archive", json!({})));
        assert_eq!(built(&["archive-verify", "a.archive"]),
            ("store.client.archive_verify", json!({"file": "a.archive"})));
        assert_eq!(built(&["archive-verify", "a.archive", "--json"]),
            ("store.client.archive_verify", json!({"file": "a.archive"})));
    }

    #[test]
    fn archive_name_good_and_bad() {
        for good in ["a.archive", "2026-11-08T00.archive", "x", "a..b", "name with space"] {
            assert!(archive_name(good).is_ok(), "{good:?} must be accepted");
            assert!(build(&parse(&["archive-verify", good]).unwrap()).is_ok(), "{good:?} must build");
        }
        for bad in ["", "a/b", "/abs", "a\\b", "..\\x", ".hidden", "..", "."] {
            let e = build(&parse(&["archive-verify", bad]).unwrap()).unwrap_err().to_string();
            assert!(!e.is_empty(), "{bad:?} must be refused with a message");
            assert!(archive_name(bad).is_err(), "{bad:?} must be refused");
        }
        assert!(archive_name("").unwrap_err().to_string().contains("empty"));
        assert!(archive_name("a/b").unwrap_err().to_string().contains("'/'"));
        assert!(archive_name(".x").unwrap_err().to_string().contains("'.'"));
    }

    #[test]
    fn archive_human_rendering_and_refusals() {
        let a = parse(&["archive"]).unwrap();
        let v = json!({"file": "f.archive", "path": "/data/f.archive", "through_seq": 42, "event_count": 7,
            "events_head": "0123456789abcdef0123", "range_sha256": "ff"});
        assert_eq!(render(&a, &v).unwrap(),
            "archived  f.archive\npath      /data/f.archive\ncovers    7 events through chain event 42\nhead      0123456789ab\n");
        for missing in ["file", "through_seq", "event_count"] {
            let mut m = v.clone();
            m.as_object_mut().unwrap().remove(missing);
            assert!(render(&a, &m).is_err(), "missing {missing} refuses");
        }
        let mut bad = v.clone();
        bad["through_seq"] = json!("42");
        assert!(render(&a, &bad).is_err(), "non-integer through_seq refuses");
        assert!(render(&a, &json!({"expired": 3})).is_err(), "sweep's field is not an archive result");
    }

    #[test]
    fn archive_verify_human_rendering_and_refusals() {
        let av = parse(&["archive-verify", "f.archive"]).unwrap();
        let v = json!({"verified": true, "through_seq": 42, "event_count": 7,
            "events_head": "0123456789abcdef", "range_sha256": "ff"});
        assert_eq!(render(&av, &v).unwrap(),
            "verified  f.archive\ncovers    7 events through chain event 42\n");
        assert!(render(&av, &json!({"verified": false, "through_seq": 42, "event_count": 7})).is_err(),
            "verified false refuses");
        assert!(render(&av, &json!({"through_seq": 42, "event_count": 7})).is_err(), "missing verified refuses");
        assert!(render(&av, &json!({"verified": "true", "through_seq": 42, "event_count": 7})).is_err(),
            "non-bool verified refuses");
        let mut no_count = v.clone();
        no_count.as_object_mut().unwrap().remove("event_count");
        assert!(render(&av, &no_count).is_err(), "missing event_count refuses");
    }

    #[test]
    fn archive_json_flag_selects_raw_output() {
        assert!(!wants_json(&parse(&["archive"]).unwrap()) && wants_json(&parse(&["archive", "--json"]).unwrap()));
        assert!(!wants_json(&parse(&["archive-verify", "f"]).unwrap())
            && wants_json(&parse(&["archive-verify", "f", "--json"]).unwrap()));
    }
}
