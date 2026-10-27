//! `weft-licence` command line.
//!
//! ```text
//! weft-licence [--config FILE] [--state-dir DIR] <command>
//!   init [--operator-key HEX]   generate the grant key (USB, once); print its fingerprint
//!   bind FILE                   apply an operator-signed binding record (USB)
//!   override FILE               install an operator-signed serve override
//!   identity                    print the fingerprint and the binding status
//!   serve                       run the listener
//! ```

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use weft_licence::bind;
use weft_licence::config::Config;
use weft_licence::keys;
use weft_licence::providers::{LocalDeclaredLicence, StubDeviceSigner};
use weft_licence::state::OperatorKeys;
use weft_licence::{Service, SvcError, artifact, http, system_clock};
use weft_licence_wire::SignedEnvelope;

const DEFAULT_CONFIG: &str = "/etc/weft-licence/config.toml";

fn usage() -> ExitCode {
    eprintln!(
        "usage: weft-licence [--config FILE] [--state-dir DIR] <init [--operator-key HEX] | bind FILE | override FILE | identity | serve>"
    );
    ExitCode::from(2)
}

fn opt(args: &mut Vec<String>, flag: &str) -> Option<String> {
    let i = args.iter().position(|a| a == flag)?;
    args.remove(i);
    (i < args.len()).then(|| args.remove(i))
}

fn load_config(path: Option<String>, state_dir: Option<String>, must_exist: bool) -> Result<Config, SvcError> {
    let p = PathBuf::from(path.clone().unwrap_or_else(|| DEFAULT_CONFIG.into()));
    let mut cfg = if p.exists() {
        Config::load(&p)?
    } else if must_exist || path.is_some() {
        return Err(SvcError::Config(format!("config {} not found", p.display())));
    } else {
        Config::default()
    };
    if let Some(d) = state_dir {
        cfg.state_dir = PathBuf::from(d);
    }
    // Files this CLI writes must be readable by the service user.
    weft_licence::fsio::require_owner(&cfg.state_dir).map_err(SvcError::Config)?;
    Ok(cfg)
}

fn read_envelope(file: &str) -> Result<SignedEnvelope, SvcError> {
    let raw = std::fs::read(file).map_err(|e| SvcError::Io(e.to_string()))?;
    serde_json::from_slice(&raw).map_err(|e| SvcError::Config(format!("{file}: {e}")))
}

fn run(mut args: Vec<String>) -> Result<(), SvcError> {
    let (config, state_dir) = (opt(&mut args, "--config"), opt(&mut args, "--state-dir"));
    let cmd = if args.is_empty() { return Err(SvcError::Config("no command".into())) } else { args.remove(0) };
    match cmd.as_str() {
        "init" => {
            let operator = opt(&mut args, "--operator-key");
            let cfg = load_config(config, state_dir, false)?;
            let r = keys::init(&cfg.state_dir)?;
            if let Some(k) = operator {
                OperatorKeys::pin(&cfg.state_dir, &k)?;
            }
            println!("grant key written: {}", r.key_path.display());
            println!("grant_pubkey:      {}", r.grant_pubkey);
            println!("fingerprint:       {}", r.fingerprint);
            println!("Compare this fingerprint with what `weaver` shows BEFORE signing the binding.");
            Ok(())
        }
        "bind" => {
            let file = args.first().ok_or_else(|| SvcError::Config("bind needs a file".into()))?;
            let cfg = load_config(config, state_dir, true)?;
            let ops = OperatorKeys::load(&cfg.state_dir, &cfg.operator_pubkeys)?;
            let cur = bind::load(&cfg.state_dir, &ops)?;
            let b = bind::apply(&cfg.state_dir, &cfg.device_id, &ops, &read_envelope(file)?, cur.as_ref())?;
            println!("binding applied: state {:?}, seq {}, mesh {}", b.record.state, b.record.seq, b.record.mesh_id);
            Ok(())
        }
        "override" => {
            let file = args.first().ok_or_else(|| SvcError::Config("override needs a file".into()))?;
            let cfg = load_config(config, state_dir, true)?;
            let ops = OperatorKeys::load(&cfg.state_dir, &cfg.operator_pubkeys)?;
            let env = read_envelope(file)?;
            artifact::verify_override(&env, &|pk| ops.contains(pk)).map_err(SvcError::Bind)?;
            artifact::install_override(&cfg.state_dir.join("overrides"), &env)?;
            println!("override installed");
            Ok(())
        }
        "identity" => {
            let cfg = load_config(config, state_dir, false)?;
            let sk = keys::load(&cfg.state_dir)?;
            let pk = sk.verifying_key().to_bytes();
            println!("fingerprint: {}", weft_licence_wire::key_id(&pk));
            println!("grant_pubkey: {}", weft_licence_wire::hex_encode(&pk));
            let ops = OperatorKeys::load(&cfg.state_dir, &cfg.operator_pubkeys)?;
            match bind::load(&cfg.state_dir, &ops)? {
                Some(b) => println!("binding: {:?} seq {} mesh {}", b.record.state, b.record.seq, b.record.mesh_id),
                None => println!("binding: none"),
            }
            Ok(())
        }
        "serve" => serve(load_config(config, state_dir, true)?),
        _ => Err(SvcError::Config(format!("unknown command {cmd}"))),
    }
}

#[cfg(feature = "registry")]
fn fetcher(cfg: &Config) -> Result<Box<dyn weft_licence::providers::CogFetcher>, SvcError> {
    use weft_licence::registry::RegistryFetcher;
    #[cfg(feature = "net")]
    let reader: Box<dyn cog_sources::Reader + Send + Sync> =
        Box::new(cog_sources::fetch::HttpReader::new());
    #[cfg(not(feature = "net"))]
    let reader: Box<dyn cog_sources::Reader + Send + Sync> = Box::new(cog_sources::FsReader);
    Ok(Box::new(RegistryFetcher::new(
        &cfg.registry_url,
        cfg.allow_insecure_registry,
        reader,
        cfg.limits.max_artifact_bytes,
    )))
}

#[cfg(not(feature = "registry"))]
fn fetcher(_: &Config) -> Result<Box<dyn weft_licence::providers::CogFetcher>, SvcError> {
    Err(SvcError::Config("built without the `registry` feature".into()))
}

fn serve(cfg: Config) -> Result<(), SvcError> {
    if cfg.listen.is_empty() {
        return Err(SvcError::Config("listen is empty; name the USB and tailnet addresses".into()));
    }
    let ops = OperatorKeys::load(&cfg.state_dir, &cfg.operator_pubkeys)?;
    let operator_ok = Box::new(move |pk: &[u8; 32]| ops.contains(pk));
    let licence = Box::new(LocalDeclaredLicence::new(cfg.licence_file.clone(), operator_ok));
    let addrs = cfg.listen.clone();
    let svc = Arc::new(Service::open(cfg.clone(), system_clock(), licence, fetcher(&cfg)?, Box::new(StubDeviceSigner))?);
    let server = http::serve(svc, &addrs).map_err(|e| SvcError::Io(e.to_string()))?;
    for a in server.addrs() {
        eprintln!("weft-licence listening on {a}");
    }
    server.wait();
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        return usage();
    }
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("weft-licence: {e}");
            ExitCode::from(1)
        }
    }
}
