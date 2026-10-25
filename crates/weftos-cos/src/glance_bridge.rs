//! The serial bridge for the e-ink glance panel: SSE in, one JSON line out per
//! change (debounced), a heartbeat so the panel can tell "idle" from "no
//! link", and idle art (`glance_art`) when nothing has happened for a while.

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Utc;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::events::{AgentState, Event};
use crate::glance::{Glance, SseParser};
use crate::glance_art::{self, ArtImage, ArtSchedule};

const DEBOUNCE: Duration = Duration::from_secs(2);
const HEARTBEAT: Duration = Duration::from_secs(60);

#[derive(clap::Args, Clone, Debug)]
pub struct GlanceSerialArgs {
    /// Serial port of the e-ink board, e.g. /dev/cu.usbmodem214301.
    #[arg(long)]
    pub port: PathBuf,
    /// Base URL of the local cos agent.
    #[arg(long, default_value = "http://127.0.0.1:18300")]
    pub url: String,
    /// Status file `devices.status` reads; defaults to the CoS config's
    /// `devices.glance_status`.
    #[arg(long)]
    pub status_file: Option<PathBuf>,
    /// CoS config file; its `art` section turns on idle art. Defaults to the
    /// standard cos.json when present.
    #[arg(long)]
    pub config: Option<PathBuf>,
}

/// What `devices.status` learns about this process: best effort, never fatal.
fn write_status(path: &Path, port: &Path, port_open: bool, last_write: Option<&str>, last_error: Option<&str>, art: serde_json::Value) {
    let body = serde_json::json!({
        "pid": std::process::id(),
        "port": port.display().to_string(),
        "port_open": port_open,
        "updated_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "last_write_at": last_write,
        "last_error": last_error,
        "art": art,
    });
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, body.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn open_port(port: &Path) -> std::io::Result<std::fs::File> {
    // No serial crate in the workspace: configure raw 115200 with stty (the
    // board's USB CDC ignores the rate, the tty layer must not cook bytes).
    let ok = std::process::Command::new("stty")
        .args(["-f"])
        .arg(port)
        .args(["115200", "raw", "-echo", "-hupcl"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        return Err(std::io::Error::other("stty failed"));
    }
    std::fs::OpenOptions::new().write(true).open(port)
}

/// SSE reader task: pushes events, and `None` whenever the link drops.
async fn read_events(url: String, tx: mpsc::Sender<Option<Event>>) {
    let http = reqwest::Client::new();
    loop {
        if let Ok(mut resp) = http.get(format!("{url}/v1/cos/events")).send().await
            && resp.status().is_success()
        {
            {
                let mut parser = SseParser::default();
                // Connected: clear a stale "link down" alert.
                let _ = tx.send(Some(Event::state(AgentState::Idle))).await;
                while let Ok(Some(chunk)) = resp.chunk().await {
                    for ev in parser.feed(&String::from_utf8_lossy(&chunk)) {
                        if tx.send(Some(ev)).await.is_err() {
                            return;
                        }
                    }
                }
            }
        }
        if tx.send(None).await.is_err() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// The `art` block of the status file.
fn art_status(enabled: bool, sched: &ArtSchedule, caption: Option<&str>, error: Option<&str>) -> serde_json::Value {
    let state = match (enabled, sched.showing, sched.in_flight) {
        (false, ..) => "off",
        (_, _, true) => "generating",
        (_, true, _) => "showing",
        _ => "waiting",
    };
    serde_json::json!({
        "state": state,
        "last_art_at": sched.last_art().map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        "caption": if sched.showing { caption } else { None },
        "last_error": error,
    })
}

pub async fn run(args: GlanceSerialArgs) -> anyhow::Result<()> {
    use std::io::Write;
    let (tx, mut rx) = mpsc::channel(256);
    tokio::spawn(read_events(args.url.trim_end_matches('/').to_string(), tx));

    let cfg = crate::CosConfig::load(args.config.as_deref()).unwrap_or_else(|e| {
        tracing::warn!("cos config: {e}; idle art off");
        crate::CosConfig::default()
    });
    let art_cfg = cfg.art.clone();
    let art_on = art_cfg.url.is_some();
    let status_path = args.status_file.clone().unwrap_or_else(|| cfg.devices.glance_status.clone());
    let art_state_path = status_path.with_file_name("glance.art.json");
    let mut sched = ArtSchedule::new(Utc::now(), glance_art::load_last_art(&art_state_path));
    let (art_tx, mut art_rx) = mpsc::channel::<(u64, Result<ArtImage, String>)>(2);
    let mut pending_art: Option<ArtImage> = None;
    let mut art_caption: Option<String> = None;
    let mut art_error: Option<String> = None;

    let mut g = Glance::default();
    let mut port: Option<std::fs::File> = None;
    let mut sent: Option<Glance> = None;
    let mut dirty = true;
    let mut deadline = Instant::now() + DEBOUNCE;
    let mut last_write = Instant::now() - HEARTBEAT;
    let mut last_write_at: Option<String> = None;
    let mut last_error: Option<String> = None;
    loop {
        let art = art_status(art_on, &sched, art_caption.as_deref(), art_error.as_deref());
        write_status(&status_path, &args.port, port.is_some(), last_write_at.as_deref(), last_error.as_deref(), art);
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(Some(ev)) => {
                    g.apply(&ev);
                    preempt_art(&mut sched, &mut g);
                    dirty = true;
                    deadline = Instant::now() + DEBOUNCE;
                }
                Some(None) => {
                    g.state = "blocked".into();
                    g.blocked_reason = "agent link down".into();
                    g.render(Utc::now());
                    preempt_art(&mut sched, &mut g);
                    dirty = true;
                    deadline = Instant::now() + DEBOUNCE;
                }
                None => return Ok(()),
            },
            Some((token, res)) = art_rx.recv() => match res {
                Ok(img) if sched.accept(token) => pending_art = Some(img),
                Ok(_) => tracing::info!("glance art: dropped, the panel was busy"),
                Err(e) => {
                    sched.accept(token);
                    sched.failed(Utc::now());
                    tracing::warn!("glance art: {e}");
                    art_error = Some(e);
                }
            },
            _ = tokio::time::sleep_until(deadline.max(Instant::now())), if dirty => {}
            _ = tokio::time::sleep(Duration::from_secs(5)) => {}
        }
        if g.tick(Utc::now()) {
            // A card expired: no event announces that.
            dirty = true;
            deadline = Instant::now() + DEBOUNCE;
        }
        let quiet = art_on && g.state == "idle" && !g.has_card();
        if sched.due(Utc::now(), quiet) {
            let token = sched.begin();
            let seed = Utc::now().timestamp_nanos_opt().unwrap_or_default() as u64;
            let (cfg, tx) = (art_cfg.clone(), art_tx.clone());
            tokio::spawn(async move {
                let scene = glance_art::scene(seed);
                let _ = tx.send((token, glance_art::generate(&cfg, &scene, seed).await)).await;
            });
        }
        let now = Instant::now();
        let changed = g.redraw || sent.as_ref().map(|s| s.content() != g.content()).unwrap_or(true);
        let due = (dirty && changed && now >= deadline) || now.duration_since(last_write) >= HEARTBEAT;
        if dirty && !changed {
            dirty = false;
        }
        if !due && pending_art.is_none() {
            continue;
        }
        if port.is_none() {
            match open_port(&args.port) {
                Ok(f) => port = Some(f),
                Err(e) => {
                    tracing::warn!("serial {}: {e}", args.port.display());
                    last_error = Some(e.to_string());
                    last_write = now - HEARTBEAT + Duration::from_secs(5);
                    deadline = now + Duration::from_secs(3);
                    continue;
                }
            }
        }
        let ts = chrono::Local::now().format("%H:%M").to_string();
        let art_img = pending_art.take();
        let line = match &art_img {
            Some(img) => img.to_line(&ts),
            None => g.to_line(&ts),
        };
        let f = port.as_mut().expect("opened above");
        if let Err(e) = f.write_all(line.as_bytes()).and_then(|_| f.flush()) {
            tracing::warn!("serial write: {e}; reopening");
            last_error = Some(e.to_string());
            port = None;
            sent = None;
            deadline = now + Duration::from_secs(3);
            continue;
        }
        last_write = now;
        last_write_at = Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
        last_error = None;
        if let Some(img) = art_img {
            tracing::info!("glance art: {}", img.caption);
            sched.shown(Utc::now());
            glance_art::save_last_art(&art_state_path, Utc::now());
            art_caption = Some(img.caption);
            art_error = None;
            // Whatever status goes out next must be sent in full.
            sent = None;
            continue;
        }
        tracing::info!("glance: {}", line.trim_end());
        g.redraw = false;
        sent = Some(g.clone());
        dirty = false;
    }
}

/// Activity takes the art down: the next status line carries `redraw` so the
/// panel repaints even when the text is unchanged.
fn preempt_art(sched: &mut ArtSchedule, g: &mut Glance) {
    if sched.activity(Utc::now()) {
        g.redraw = true;
    }
}
