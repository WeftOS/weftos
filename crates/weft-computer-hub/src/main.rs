//! `weft-hub`: serve the computer hub, mint pass keys, generate minter keys.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use weft_hub::config::{HubConfig, generate_key_hex, write_secret};
use weft_hub::hub::{Hub, now_secs};
use weft_hub::passkey::{self, Role};
use weft_hub::server;

const USAGE: &str = "\
weft-hub serve   [--config <hub.toml>, default $WEFT_HUB_HOME/hub.toml, else $XDG_CONFIG_HOME/weft-hub/hub.toml, else $HOME/.config/weft-hub/hub.toml] [--addr ip:port] [--allow-non-loopback]
weft-hub mint    --config <hub.toml> --minter <id> --sub <user> --role server|harness[,..]
                 [--srv <machine id>] [--ttl <secs, default 3600>] [--out <file>]
weft-hub keygen  --out <file>
weft-hub print-app";

/// Parse `--flag value` pairs and bare flags. Unknown or repeated flags refuse.
fn parse_flags(
    args: &[String],
    valued: &[&str],
    bare: &[&str],
) -> Result<HashMap<String, String>, String> {
    let mut out = HashMap::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let name = a
            .strip_prefix("--")
            .ok_or_else(|| format!("unexpected argument {a:?}"))?;
        let value = if valued.contains(&name) {
            it.next()
                .cloned()
                .ok_or_else(|| format!("--{name} needs a value"))?
        } else if bare.contains(&name) {
            String::new()
        } else {
            return Err(format!("unknown flag --{name}"));
        };
        if out.insert(name.to_owned(), value).is_some() {
            return Err(format!("--{name} given twice"));
        }
    }
    Ok(out)
}

fn need<'a>(f: &'a HashMap<String, String>, k: &str) -> Result<&'a str, String> {
    f.get(k)
        .map(String::as_str)
        .ok_or_else(|| format!("--{k} is required"))
}

fn cmd_keygen(args: &[String]) -> Result<(), String> {
    let f = parse_flags(args, &["out"], &[])?;
    let hex = generate_key_hex()?;
    write_secret(Path::new(need(&f, "out")?), &format!("{hex}\n"))
}

fn cmd_mint(args: &[String]) -> Result<(), String> {
    let f = parse_flags(
        args,
        &["config", "minter", "sub", "role", "srv", "ttl", "out"],
        &[],
    )?;
    let cfg = HubConfig::load(Path::new(need(&f, "config")?)).map_err(|e| e.to_string())?;
    let minter = cfg
        .minters
        .get(need(&f, "minter")?)
        .ok_or_else(|| "unknown --minter".to_owned())?;
    let roles: Vec<Role> = need(&f, "role")?
        .split(',')
        .map(|r| {
            Role::parse(r.trim()).ok_or_else(|| format!("unknown role {r:?} (server, harness)"))
        })
        .collect::<Result<_, _>>()?;
    let ttl: u64 = f.get("ttl").map_or(Ok(3600), |t| {
        t.parse().map_err(|_| "--ttl must be seconds".to_owned())
    })?;
    if ttl > cfg.max_ttl_secs {
        return Err(format!(
            "--ttl {ttl} exceeds the hub's max_ttl_secs {}",
            cfg.max_ttl_secs
        ));
    }
    if let Some(srv) = f.get("srv")
        && !cfg.machines.contains_key(srv)
    {
        return Err(format!("--srv {srv:?} is not a configured machine"));
    }
    let token = passkey::mint(
        minter,
        need(&f, "sub")?,
        &roles,
        ttl,
        f.get("srv").map(String::as_str),
        now_secs(),
    )
    .map_err(|e| e.to_string())?;
    match f.get("out") {
        Some(p) => write_secret(Path::new(p), &format!("{token}\n")),
        None => {
            println!("{token}");
            Ok(())
        }
    }
}

/// Default config location. Never under `~/.weftos` (the daemon's home).
fn default_config_path() -> Result<PathBuf, String> {
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    if let Some(h) = env("WEFT_HUB_HOME") {
        return Ok(h.join("hub.toml"));
    }
    if let Some(x) = env("XDG_CONFIG_HOME") {
        return Ok(x.join("weft-hub/hub.toml"));
    }
    let home = env("HOME").ok_or("--config not given and none of WEFT_HUB_HOME, XDG_CONFIG_HOME, HOME is set")?;
    Ok(home.join(".config/weft-hub/hub.toml"))
}

async fn cmd_serve(args: &[String]) -> Result<(), String> {
    let f = parse_flags(args, &["config", "addr"], &["allow-non-loopback"])?;
    let cfg_path = match f.get("config") {
        Some(p) => PathBuf::from(p),
        None => default_config_path()?,
    };
    let cfg = HubConfig::load(&cfg_path).map_err(|e| e.to_string())?;
    let addr = f.get("addr").cloned().unwrap_or_else(|| cfg.addr.clone());
    let sa = server::check_bind(&addr, f.contains_key("allow-non-loopback") || cfg.allow_non_loopback)?;
    let listener = tokio::net::TcpListener::bind(sa)
        .await
        .map_err(|e| format!("bind {sa}: {e}"))?;
    tracing::info!(%sa, machines = cfg.machines.len(), minters = cfg.minters.len(), "weft-hub listening");
    let hub = Hub::new(cfg);
    server::serve(hub, listener, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await;
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = match args.split_first() {
        Some((c, r)) => (c.as_str(), r),
        None => ("", &args[..]),
    };
    let result = match cmd {
        "serve" => cmd_serve(rest).await,
        "mint" => cmd_mint(rest),
        "keygen" => cmd_keygen(rest),
        "print-app" => {
            print!("{}", weft_hub::supervision::APP_MANIFEST);
            Ok(())
        }
        _ => Err(format!("unknown or missing command\n{USAGE}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("weft-hub: {e}");
            ExitCode::from(2)
        }
    }
}
