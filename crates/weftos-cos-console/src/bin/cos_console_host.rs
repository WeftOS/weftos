//! `cos-console-host`: the chief-of-staff console in a Mac window,
//! talking to the agent (`weft cos serve --mic`) over its loopback API.
//!
//! ```bash
//! cos-console-host --window                 # needs the `window` feature (SDL2)
//! cos-console-host --window --scale 2 --agent http://127.0.0.1:18300
//! cos-console-host --sim --seconds 2        # headless smoke run, no SDL
//! cos-console-host --leaf <leaf id>         # the edge pad, through the daemon
//! cos-console-host --leaf <leaf id> --daemon /path/to/kernel.sock
//! ```
//!
//! `--leaf` opens a display session on that certified leaf through the
//! daemon's `display.*` RPC (ADR-117): the console draws on the pad, and the
//! pad's touch and push-to-talk button come back as events. The daemon
//! socket defaults to `$WEFTOS_RUNTIME_DIR/kernel.sock`, else
//! `~/.clawft/kernel.sock`.
//!
//! Hold the space bar to talk. Enter opens a typed line for development
//! (Enter sends, Escape cancels); `--no-typing` turns that off.

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use weftos_cos_console::frame::SUBJECT;
use weftos_cos_console::host::supervise::{supervise, SuperviseConfig};
use weftos_cos_console::host::{HostConfig, DEFAULT_AGENT};
use weftos_display_client::{DisplayError, DisplayGrant, DisplaySession, DisplayTarget, LeafEndpoint, WindowConfig};

struct Args {
    window: bool,
    scale: u32,
    agent: String,
    typing: bool,
    seconds: Option<u64>,
    leaf: Option<String>,
    daemon: Option<std::path::PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args { window: cfg!(feature = "window"), scale: 1, agent: DEFAULT_AGENT.into(), typing: true, seconds: None, leaf: None, daemon: None };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--window" => a.window = true,
            "--sim" => a.window = false,
            "--no-typing" => a.typing = false,
            "--scale" => a.scale = it.next().and_then(|v| v.parse().ok()).ok_or("--scale <n>")?,
            "--agent" => a.agent = it.next().ok_or("--agent <url>")?,
            "--leaf" => a.leaf = Some(it.next().ok_or("--leaf <leaf id>")?),
            "--daemon" => a.daemon = Some(it.next().ok_or("--daemon <socket path>")?.into()),
            "--seconds" => a.seconds = Some(it.next().and_then(|v| v.parse().ok()).ok_or("--seconds <n>")?),
            "--help" | "-h" => {
                eprintln!("usage: cos-console-host [--window|--sim|--leaf ID [--daemon SOCKET]] [--scale N] [--agent URL] [--no-typing] [--seconds N]");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(a)
}

fn open_session(args: &Args) -> Result<DisplaySession, DisplayError> {
    let target = if let Some(id) = &args.leaf {
        DisplayTarget::Leaf(LeafEndpoint {
            name: format!("weftos://local/nodes/{id}/services/display"),
            leaf_id: id.clone(),
            display: 0,
            daemon: args.daemon.clone(),
        })
    } else if args.window {
        if !cfg!(feature = "window") {
            return Err(DisplayError::NotYet("built without the `window` feature; run scripts/build.sh cos-host"));
        }
        DisplayTarget::Window(WindowConfig { title: "chief of staff".into(), scale: args.scale, ..WindowConfig::default() })
    } else {
        DisplayTarget::sim().0
    };
    DisplaySession::open(target, DisplayGrant::dev(SUBJECT))
}

fn main() -> Result<(), String> {
    let args = parse_args()?;
    let host = HostConfig { agent: args.agent.clone(), typing: args.typing, ..HostConfig::default() };
    let mut cfg = SuperviseConfig::new(host);
    // A leaf (or the daemon behind it) can drop and come back; a window
    // the user closed is the end.
    cfg.reopen_on_close = args.leaf.is_some();
    cfg.limit = args.seconds.map(Duration::from_secs);
    eprintln!("console host: agent {}  (space = talk{})", args.agent, if args.typing { ", Enter = type" } else { "" });

    // Bad arguments were refused above; from here on only SIGINT/SIGTERM
    // (default disposition) ends the process, plus --seconds and a
    // closed window.
    let stop = AtomicBool::new(false);
    supervise(
        || open_session(&args),
        || chrono::Local::now().format("%H:%M").to_string(),
        &cfg,
        &stop,
        |line| eprintln!("{line}"),
    );
    Ok(())
}
