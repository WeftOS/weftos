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
use crate::voice::{PttControl, PttEdge, PttOutcome, PushToTalk, Retention};
use crate::speak::{self, Speaker};
use crate::endpoints::{self, TokenStore};
use crate::{CosConfig, host, model};

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
    /// Start the local model server first when it is down (detached), wait
    /// for it, and stop it again when this process exits.
    #[arg(long)]
    pub start_model: bool,
}

#[derive(clap::Subcommand, Clone, Debug)]
pub enum ModelCmd {
    /// Probe the local model server and name the cloud provider, if keyed.
    Status {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Start the local model server detached and wait until it answers.
    Start {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Stop the server this host started (by its recorded pid only).
    Stop,
}

#[derive(clap::Subcommand, Clone, Debug)]
pub enum EndpointCmd {
    /// Create a remote endpoint: writes its bearer token to a 0600 file and
    /// prints only the file's path.
    Add {
        name: String,
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// List the remote endpoints that have a token.
    List {
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

/// `weftos-cos endpoint ...`
pub fn endpoint(cmd: EndpointCmd) -> anyhow::Result<()> {
    match cmd {
        EndpointCmd::Add { name, config } => {
            let cfg = CosConfig::load(config.as_deref())?;
            let path = TokenStore::new(&cfg.serve.tokens_dir).add(&name)?;
            println!("{}", path.display());
            Ok(())
        }
        EndpointCmd::List { config } => {
            let cfg = CosConfig::load(config.as_deref())?;
            for n in TokenStore::new(&cfg.serve.tokens_dir).names() {
                println!("{n}");
            }
            Ok(())
        }
    }
}

/// `weftos-cos model ...`
pub async fn model(cmd: ModelCmd) -> anyhow::Result<()> {
    match cmd {
        ModelCmd::Status { config } => {
            let cfg = CosConfig::load(config.as_deref())?;
            let built = host::build(&cfg).await?;
            report_models(&built).await;
            Ok(())
        }
        ModelCmd::Start { config } => {
            let cfg = CosConfig::load(config.as_deref())?;
            let built = host::build(&cfg).await?;
            let started = ensure_local_model(&cfg, &built).await?;
            eprintln!("{}", if started { "local model started" } else { "local model already up (or not needed)" });
            Ok(())
        }
        ModelCmd::Stop => {
            println!("{}", model::stop_started(&model::state_dir())?);
            Ok(())
        }
    }
}

/// Models, their health and the tools that did not bind, on stderr.
pub async fn report_models(built: &host::Built) {
    eprintln!(
        "chief of staff: quick={} plan={} fallback={}",
        built.quick_model,
        built.plan_model,
        if built.fallback_models.is_empty() { "none".to_string() } else { built.fallback_models.join(" > ") }
    );
    match &built.local_api_base {
        Some(base) => match model::probe(base).await {
            model::Health::Up => eprintln!("local model server: up at {base}"),
            model::Health::Down(why) => eprintln!(
                "local model server: DOWN ({why}); start it with `weftos-cos model start` or `serve --start-model`{}",
                if !built.fallback_models.is_empty() { "; turns fall back to the cloud model meanwhile" } else { "" }
            ),
        },
        None => eprintln!("local model server: not used"),
    }
    if built.cloud_providers.is_empty() {
        eprintln!("cloud providers: none keyed");
    } else {
        eprintln!("cloud providers with a key (presence only, not tested): {}", built.cloud_providers.join(", "));
    }
    for (tool, reason) in &built.unbound {
        eprintln!("not bound: {tool}: {reason}");
    }
    if let Some(b) = &built.brain {
        eprintln!("brain: {} ({} entries) at {}", b.installation(), b.len(), b.path().display());
    }
}

/// Start the local server when it is needed and down. Returns whether this
/// call started it.
async fn ensure_local_model(cfg: &CosConfig, built: &host::Built) -> anyhow::Result<bool> {
    let Some(base) = &built.local_api_base else { return Ok(false) };
    if model::probe(base).await.is_up() {
        return Ok(false);
    }
    let argv = model::serve_argv(&cfg.models, &built.quick_model);
    let pid = model::start_detached(&argv, &model::state_dir())?;
    eprintln!("starting local model (pid {pid}, log {}/model.log)", model::state_dir().display());
    if let model::Health::Down(why) = model::wait_up(base, std::time::Duration::from_secs(120)).await {
        anyhow::bail!("local model did not come up: {why}");
    }
    eprintln!("loading {} (first load takes about a minute)", built.quick_model);
    match model::warm(base, &built.quick_model, std::time::Duration::from_secs(300)).await {
        model::Health::Up => Ok(true),
        model::Health::Down(why) => anyhow::bail!("local model did not load: {why}"),
    }
}

/// Shared by `weftos-cos serve` and `weft cos serve`.
pub async fn serve(args: ServeArgs) -> anyhow::Result<()> {
    let mut cfg = CosConfig::load(args.config.as_deref())?;
    if let Some(bind) = args.bind {
        cfg.bind = bind;
    }
    let addr = cfg.bind_addr()?;
    let built = host::build(&cfg).await?;
    // Stops the server this call started on every way out (a bind failure,
    // an error, Ctrl-C), never one that was already running.
    let _model_guard = StartedModel(if args.start_model { ensure_local_model(&cfg, &built).await? } else { false });
    report_models(&built).await;
    let conv: Arc<dyn Conversation> = Arc::new(built.agent.clone());
    let speaker = speak::engine_from_config(&cfg.voice.tts, speak::say_exists())?.map(|e| Arc::new(Speaker::new(e)));
    match &speaker {
        Some(s) => eprintln!("spoken replies: {}", s.engine_name()),
        None => eprintln!("spoken replies: off"),
    }
    built.agent.set_silent_endpoints(cfg.silent_endpoints());
    let speaking = speaker.clone().map(|s| tokio::spawn(speak::speak_replies(conv.events(), built.agent.clone(), s)));
    // One push-to-talk owner, shared by the terminal and `POST /v1/cos/ptt`
    // (the console host's space bar or the pad button).
    let ptt = args.mic.then(|| {
        let p = build_ptt(&cfg, conv.clone());
        Arc::new(match &speaker {
            Some(s) => p.with_barge_in(s.clone()),
            None => p,
        })
    });
    let control: Option<Arc<dyn PttControl>> = ptt.clone().map(|p| p as Arc<dyn PttControl>);

    let server = match cfg.tailnet_port()? {
        None => {
            let (local, server) = crate::server::bind_with(conv.clone(), control, addr).await?;
            eprintln!("conversation API on http://{local}  (POST /v1/cos/utterance, POST /v1/cos/ptt, GET /v1/cos/events)");
            futures_util::future::Either::Left(server)
        }
        Some(port) => {
            let ip = endpoints::tailnet_ip()?;
            let tokens = TokenStore::new(&cfg.serve.tokens_dir);
            let (bound, server) = crate::server::bind_remote(conv.clone(), control, addr, std::net::SocketAddr::new(ip.into(), port), tokens.clone()).await?;
            eprintln!("conversation API on http://{} (no token) and http://{} (bearer token per endpoint)", bound.loopback, bound.tailnet);
            let names = tokens.names();
            if names.is_empty() {
                eprintln!("no remote endpoints yet; create one with `weftos-cos endpoint add <name>`");
            } else {
                eprintln!("remote endpoints: {}", names.join(", "));
            }
            futures_util::future::Either::Right(server)
        }
    };

    let printer = tokio::spawn(print_events(conv.events()));
    let stdin_loop = if args.no_stdin { None } else { Some(tokio::spawn(terminal(conv.clone(), ptt))) };

    tokio::select! {
        r = server => { r?; }
        _ = shutdown_signal() => { eprintln!("stopping"); }
    }
    printer.abort();
    if let Some(h) = speaking {
        h.abort();
    }
    if let Some(h) = stdin_loop {
        h.abort();
    }
    Ok(())
}

struct StartedModel(bool);

impl Drop for StartedModel {
    fn drop(&mut self) {
        if self.0 {
            match model::stop_started(&model::state_dir()) {
                Ok(m) => eprintln!("local model: {m}"),
                Err(e) => eprintln!("local model: {e}"),
            }
        }
    }
}

/// Ctrl-C, or SIGTERM from a service manager or a script.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
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

/// The STT backend: `voice.stt_model_dir`, else `$WEFTOS_PARAKEET_DIR`, else
/// the loose bundle in `~/.weftos/models/parakeet` when it is there, else the
/// backend's own discovery.
pub fn build_stt(cfg: &CosConfig) -> Arc<dyn clawft_channels::voice::stt::SttBackend> {
    use clawft_voice_onnx::parakeet::ParakeetStt;
    if let Some(dir) = &cfg.voice.stt_model_dir {
        return Arc::new(ParakeetStt::with_dir(dir));
    }
    if std::env::var_os("WEFTOS_PARAKEET_DIR").is_none()
        && let Some(loose) = dirs::home_dir().map(|h| h.join(".weftos/models/parakeet")).filter(|d| d.join("encoder.int8.onnx").exists())
    {
        return Arc::new(ParakeetStt::with_dir(loose));
    }
    Arc::new(ParakeetStt::new())
}

/// `weftos-cos transcribe <wav>`: run the configured STT over a file, to
/// check the voice path without a microphone.
pub async fn transcribe(wav: PathBuf, config: Option<PathBuf>) -> anyhow::Result<()> {
    let cfg = CosConfig::load(config.as_deref())?;
    let samples = crate::voice::read_wav_16k(&wav)?;
    let utt = clawft_channels::voice::stt::Utterance { samples, sample_rate: 16_000 };
    let text = build_stt(&cfg).transcribe(&utt).await.map_err(|e| anyhow::anyhow!("stt: {e}"))?;
    println!("{}", text.trim());
    Ok(())
}

/// Push-to-talk over the configured STT bundle and input device.
fn build_ptt(cfg: &CosConfig, conv: Arc<dyn Conversation>) -> PushToTalk {
    let stt = build_stt(cfg);
    let retention = Retention { keep_audio_debug: cfg.voice.keep_audio_debug, debug_audio_dir: cfg.voice.debug_audio_dir.clone() };
    PushToTalk::new(stt, conv).with_retention(retention).with_device(cfg.voice.device.clone())
}

async fn terminal(conv: Arc<dyn Conversation>, ptt: Option<Arc<PushToTalk>>) {
    if ptt.is_some() {
        eprintln!("push-to-talk: press Enter to start capturing, Enter again to stop; type a line to send text");
    }
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
