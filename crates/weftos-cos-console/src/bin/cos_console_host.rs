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

use std::time::{Duration, Instant};

use weftos_cos_console::frame::SUBJECT;
use weftos_cos_console::host::{Host, HostConfig, DEFAULT_AGENT};
use weftos_display_client::{DisplayGrant, DisplaySession, DisplayTarget, LeafEndpoint, WindowConfig};

const FRAME: Duration = Duration::from_millis(33);

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

fn open_session(args: &Args) -> Result<DisplaySession, String> {
    let target = if let Some(id) = &args.leaf {
        DisplayTarget::Leaf(LeafEndpoint {
            name: format!("weftos://local/nodes/{id}/services/display"),
            leaf_id: id.clone(),
            display: 0,
            daemon: args.daemon.clone(),
        })
    } else if args.window {
        if !cfg!(feature = "window") {
            return Err("built without the `window` feature; run scripts/build.sh cos-host".into());
        }
        DisplayTarget::Window(WindowConfig { title: "chief of staff".into(), scale: args.scale, ..WindowConfig::default() })
    } else {
        DisplayTarget::sim().0
    };
    DisplaySession::open(target, DisplayGrant::dev(SUBJECT)).map_err(|e| e.to_string())
}

fn main() -> Result<(), String> {
    let args = parse_args()?;
    let mut session = open_session(&args)?;
    let cfg = HostConfig { agent: args.agent.clone(), typing: args.typing, ..HostConfig::default() };
    let mut host = Host::open(&mut session, cfg).map_err(|e| e.to_string())?;
    eprintln!("console host: agent {}  (space = talk{})", args.agent, if args.typing { ", Enter = type" } else { "" });

    let start = Instant::now();
    let limit = args.seconds.map(Duration::from_secs);
    loop {
        let frame_start = Instant::now();
        let now_ms = start.elapsed().as_millis() as u32;
        host.set_clock(&chrono::Local::now().format("%H:%M").to_string()).map_err(|e| e.to_string())?;
        if !host.step(now_ms).map_err(|e| e.to_string())? {
            break;
        }
        if limit.is_some_and(|l| start.elapsed() >= l) {
            break;
        }
        if let Some(rest) = FRAME.checked_sub(frame_start.elapsed()) {
            std::thread::sleep(rest);
        }
    }
    Ok(())
}
