//! The `serve` command shared by the `weftos-cos` binary and `weft cos serve`:
//! start the chief-of-staff agent and its loopback API.
//!
//! Development affordances on the terminal: typed lines become `typed`
//! utterances; with `--mic` (feature `mic`) an empty line toggles push-to-talk
//! (press Enter to start capturing, Enter again to stop and transcribe).
//! Events are printed as they stream.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, BufReader};

use crate::conversation::{Conversation, UserUtterance};
use crate::events::Event;
use crate::voice::{PttEdge, PttOutcome, PushToTalk, Retention};
use crate::{CosConfig, host};

#[derive(clap::Args, Clone, Debug, Default)]
pub struct ServeArgs {
    /// Host config (default ~/.config/weftos/cos.json when present).
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Override the loopback bind address.
    #[arg(long)]
    pub bind: Option<String>,
    /// Push-to-talk from the terminal: Enter starts and stops a capture.
    #[arg(long)]
    pub mic: bool,
    /// Do not read the terminal; serve the API only.
    #[arg(long)]
    pub no_stdin: bool,
}

/// Shared by `weftos-cos serve` and `weft cos serve`.
pub async fn serve(args: ServeArgs) -> anyhow::Result<()> {
    let mut cfg = CosConfig::load(args.config.as_deref())?;
    if let Some(bind) = args.bind {
        cfg.bind = bind;
    }
    let addr = cfg.bind_addr()?;
    let built = host::build(&cfg).await?;
    eprintln!("chief of staff: quick={} plan={}", built.quick_model, built.plan_model);
    for (tool, reason) in &built.unbound {
        eprintln!("tool {tool} not bound: {reason}");
    }
    let conv: Arc<dyn Conversation> = Arc::new(built.agent.clone());

    let (local, server) = crate::server::bind(conv.clone(), addr).await?;
    eprintln!("conversation API on http://{local}  (POST /v1/cos/utterance, GET /v1/cos/events)");

    let printer = tokio::spawn(print_events(conv.events()));
    let stdin_loop = if args.no_stdin { None } else { Some(tokio::spawn(terminal(conv.clone(), cfg.clone(), args.mic))) };

    tokio::select! {
        r = server => { r?; }
        _ = tokio::signal::ctrl_c() => { eprintln!("stopping"); }
    }
    printer.abort();
    if let Some(h) = stdin_loop {
        h.abort();
    }
    Ok(())
}

async fn print_events(mut rx: tokio::sync::broadcast::Receiver<Event>) {
    loop {
        match rx.recv().await {
            Ok(Event::Token { .. }) => {}
            Ok(Event::Message { text, .. }) => eprintln!("\n[cos] {text}"),
            Ok(Event::Tool { name, summary, status }) => eprintln!("[tool] {name} {status}: {summary}"),
            Ok(Event::Status { task, model, lanes_running }) => eprintln!("[status] {task} · {model} · lanes {lanes_running}"),
            Ok(Event::State { state, reason }) => eprintln!("[state] {state:?}{}", reason.map(|r| format!(": {r}")).unwrap_or_default()),
            Ok(Event::Page { .. }) => eprintln!("[page]"),
            Ok(Event::Layout { layout }) => eprintln!("[layout] {layout:?}"),
            Ok(Event::Checklist { title, items, .. }) => eprintln!("[checklist] {title}: {}/{}", items.iter().filter(|i| i.done).count(), items.len()),
            Ok(Event::Steps { title, current, steps, .. }) => eprintln!("[steps] {title}: {}/{}", current + 1, steps.len()),
            Ok(Event::Ask { question, choices, index, total, .. }) => {
                eprintln!("[ask] Q{index}/{total} {question}");
                for c in choices {
                    eprintln!("       {} {}", c.n, c.caption);
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => break,
        }
    }
}

async fn terminal(conv: Arc<dyn Conversation>, cfg: CosConfig, mic: bool) {
    let ptt = if mic {
        let stt: Arc<dyn clawft_channels::voice::stt::SttBackend> = Arc::new(match &cfg.voice.stt_model_dir {
            Some(dir) => clawft_voice_onnx::parakeet::ParakeetStt::with_dir(dir),
            None => clawft_voice_onnx::parakeet::ParakeetStt::new(),
        });
        let retention = Retention { keep_audio_debug: cfg.voice.keep_audio_debug, debug_audio_dir: cfg.voice.debug_audio_dir.clone() };
        eprintln!("push-to-talk: press Enter to start capturing, Enter again to stop; type a line to send text");
        Some(PushToTalk::new(stt, conv.clone()).with_retention(retention).with_device(cfg.voice.device.clone()))
    } else {
        None
    };
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let text = line.trim();
        if text.is_empty() {
            let Some(ptt) = &ptt else { continue };
            let edge = if ptt.is_active() { PttEdge::Up } else { PttEdge::Down };
            match ptt.on_edge(edge).await {
                Ok(None) => eprintln!("[ptt] capturing… press Enter to stop"),
                Ok(Some(PttOutcome::Posted { text, .. })) => eprintln!("[you, voice] {text}"),
                Ok(Some(PttOutcome::Nothing { reason })) => eprintln!("[ptt] nothing posted: {reason}"),
                Err(e) => eprintln!("[ptt] {e}"),
            }
            continue;
        }
        if let Err(e) = conv.utter(UserUtterance::typed(text)).await {
            eprintln!("[cos] refused: {e}");
        }
    }
}
