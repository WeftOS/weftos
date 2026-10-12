//! Spoken replies (contract §4, the other half of push-to-talk): the final
//! message of a voice turn is read aloud by a local engine.
//!
//! Engines, chosen by `voice.tts.engine`:
//! - `say`: the Mac's built-in `say`. Works with no models and no network;
//!   the default on macOS.
//! - `command`: any argv that reads the text on stdin and plays it. This is
//!   the seam for the heavier local engines (Kokoro, Pocket TTS, Orpheus via
//!   Ollama) once a wrapper script exists; nothing here depends on them.
//! - `none`: silent.
//!
//! Only voice turns of the `host` endpoint are spoken here; a remote endpoint
//! (the glasses) gets the reply flagged `speak` and speaks it itself.
//! Only voice turns are spoken. A new push-to-talk press interrupts the
//! reply in progress (barge-in).

use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Notify, broadcast};

use crate::CosError;
use crate::agent::CosAgent;
use crate::config::TtsConfig;
use crate::events::Event;

/// Longest reply read aloud; the rest stays on the screen.
const MAX_SPOKEN_CHARS: usize = 700;

#[async_trait]
pub trait Speak: Send + Sync {
    /// Speak `text` and return when it has finished. Dropping the future
    /// stops the speech.
    async fn speak(&self, text: &str) -> Result<(), CosError>;
    fn name(&self) -> String;
}

pub struct SayEngine {
    pub voice: Option<String>,
    pub rate: Option<u32>,
}

#[async_trait]
impl Speak for SayEngine {
    async fn speak(&self, text: &str) -> Result<(), CosError> {
        let mut cmd = tokio::process::Command::new("say");
        if let Some(v) = &self.voice {
            cmd.args(["-v", v]);
        }
        if let Some(r) = self.rate {
            cmd.args(["-r", &r.to_string()]);
        }
        cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true);
        run_with_stdin(cmd, text, "say").await
    }

    fn name(&self) -> String {
        format!("say{}", self.voice.as_ref().map(|v| format!(" ({v})")).unwrap_or_default())
    }
}

pub struct CommandEngine {
    pub argv: Vec<String>,
}

#[async_trait]
impl Speak for CommandEngine {
    async fn speak(&self, text: &str) -> Result<(), CosError> {
        let (prog, args) = self.argv.split_first().ok_or_else(|| CosError::Voice("voice.tts.command is empty".into()))?;
        let mut cmd = tokio::process::Command::new(prog);
        cmd.args(args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true);
        run_with_stdin(cmd, text, prog).await
    }

    fn name(&self) -> String {
        format!("command ({})", self.argv.first().map(String::as_str).unwrap_or("?"))
    }
}

async fn run_with_stdin(mut cmd: tokio::process::Command, text: &str, what: &str) -> Result<(), CosError> {
    let mut child = cmd.spawn().map_err(|e| CosError::Voice(format!("start {what}: {e}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes()).await.map_err(|e| CosError::Voice(format!("{what} stdin: {e}")))?;
        // Closing stdin tells the engine the text is complete.
        drop(stdin);
    }
    let status = child.wait().await.map_err(|e| CosError::Voice(format!("{what}: {e}")))?;
    if status.success() { Ok(()) } else { Err(CosError::Voice(format!("{what} exited with {status}"))) }
}

/// The engine `cfg` asks for, or `None` when speech is off. `auto` is `say`
/// where it exists. A `command` engine without a command is refused.
pub fn engine_from_config(cfg: &TtsConfig, say_exists: bool) -> Result<Option<Arc<dyn Speak>>, CosError> {
    match cfg.engine.as_str() {
        "none" => Ok(None),
        "say" => Ok(Some(Arc::new(SayEngine { voice: cfg.voice.clone(), rate: cfg.rate }))),
        "auto" => Ok(say_exists.then(|| Arc::new(SayEngine { voice: cfg.voice.clone(), rate: cfg.rate }) as Arc<dyn Speak>)),
        "command" => match &cfg.command {
            Some(argv) if !argv.is_empty() => Ok(Some(Arc::new(CommandEngine { argv: argv.clone() }))),
            _ => Err(CosError::Config("voice.tts.engine is `command` but voice.tts.command is not set".into())),
        },
        other => Err(CosError::Config(format!("voice.tts.engine {other:?} is not one of auto, say, command, none"))),
    }
}

pub fn say_exists() -> bool {
    std::path::Path::new("/usr/bin/say").exists()
}

/// Text fit to be read aloud: markdown marks removed, one line, clipped at a
/// sentence end.
pub fn spoken_text(text: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        let l = line.trim().trim_start_matches(['#', '>', '-', '*', ' ']);
        // "1. " or "2) " list numbering; a bare leading number is kept.
        let digits = l.chars().take_while(char::is_ascii_digit).count();
        let l = if digits > 0 && l[digits..].starts_with(['.', ')']) { l[digits..].trim_start_matches(['.', ')', ' ']) } else { l };
        if l.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(l);
    }
    // [text](url) -> text
    let mut plain = String::new();
    let mut rest = out.as_str();
    while let Some(open) = rest.find('[') {
        match (rest[open..].find("]("), rest[open..].find(')')) {
            (Some(mid), Some(close)) if mid < close => {
                plain.push_str(&rest[..open]);
                plain.push_str(&rest[open + 1..open + mid]);
                rest = &rest[open + close + 1..];
            }
            _ => {
                plain.push_str(&rest[..=open]);
                rest = &rest[open + 1..];
            }
        }
    }
    plain.push_str(rest);
    let plain: String = plain.chars().filter(|c| !matches!(c, '*' | '`' | '_' | '#')).collect();
    if plain.chars().count() <= MAX_SPOKEN_CHARS {
        return plain;
    }
    let head: String = plain.chars().take(MAX_SPOKEN_CHARS).collect();
    match head.rfind(['.', '!', '?']) {
        Some(i) if i > 80 => head[..=i].to_string(),
        _ => head,
    }
}

/// Reads voice-turn replies aloud, one at a time, and can be interrupted.
pub struct Speaker {
    engine: Arc<dyn Speak>,
    interrupt: Notify,
}

impl Speaker {
    pub fn new(engine: Arc<dyn Speak>) -> Self {
        Self { engine, interrupt: Notify::new() }
    }

    pub fn engine_name(&self) -> String {
        self.engine.name()
    }

    /// Stop whatever is being spoken now (barge-in).
    pub fn interrupt(&self) {
        self.interrupt.notify_waiters();
    }

    /// Speak `text`; returns early when interrupted.
    pub async fn say(&self, text: &str) -> Result<(), CosError> {
        let spoken = spoken_text(text);
        if spoken.is_empty() {
            return Ok(());
        }
        tokio::select! {
            r = self.engine.speak(&spoken) => r,
            _ = self.interrupt.notified() => Ok(()),
        }
    }
}

/// Follow the event stream and speak the final reply of each voice turn.
/// Runs until the stream closes.
pub async fn speak_replies(mut rx: broadcast::Receiver<Event>, agent: CosAgent, speaker: Arc<Speaker>) {
    loop {
        match rx.recv().await {
            Ok(Event::Message { turn_id, text, is_final: true, .. }) if agent.host_speaks(&turn_id) => {
                if let Err(e) = speaker.say(&text).await {
                    tracing::warn!(error = %e, "spoken reply failed");
                }
            }
            Ok(_) => {}
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_is_stripped_for_speech() {
        let t = spoken_text("**Where we are**\n- C3 is done\n- [the pad](https://x.invalid/p) is next\n1. Ship it");
        assert_eq!(t, "Where we are C3 is done the pad is next Ship it");
    }

    #[test]
    fn a_leading_number_that_is_not_list_numbering_stays() {
        assert_eq!(spoken_text("3 nodes are healthy"), "3 nodes are healthy");
    }

    #[test]
    fn long_replies_are_clipped_at_a_sentence() {
        let long = "This is a sentence about the plan. ".repeat(40);
        let t = spoken_text(&long);
        assert!(t.chars().count() <= MAX_SPOKEN_CHARS);
        assert!(t.ends_with('.'));
    }

    #[test]
    fn engine_choice_and_refusals() {
        let mut cfg = TtsConfig::default();
        assert!(engine_from_config(&cfg, true).unwrap().is_some());
        assert!(engine_from_config(&cfg, false).unwrap().is_none());
        cfg.engine = "none".into();
        assert!(engine_from_config(&cfg, true).unwrap().is_none());
        cfg.engine = "command".into();
        assert!(matches!(engine_from_config(&cfg, true), Err(CosError::Config(_))));
        cfg.engine = "festival".into();
        assert!(matches!(engine_from_config(&cfg, true), Err(CosError::Config(_))));
    }

    struct Slow;
    #[async_trait]
    impl Speak for Slow {
        async fn speak(&self, _: &str) -> Result<(), CosError> {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            Ok(())
        }
        fn name(&self) -> String {
            "slow".into()
        }
    }

    #[tokio::test]
    async fn an_interrupt_ends_speech_early() {
        let s = Arc::new(Speaker::new(Arc::new(Slow)));
        let s2 = s.clone();
        let t = tokio::spawn(async move { s2.say("a long reply").await });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        s.interrupt();
        let r = tokio::time::timeout(std::time::Duration::from_secs(2), t).await.expect("interrupted").unwrap();
        assert!(r.is_ok());
    }

    #[tokio::test]
    async fn a_command_engine_gets_the_text_on_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("spoken.txt");
        let e = CommandEngine { argv: vec!["/bin/sh".into(), "-c".into(), format!("cat > {}", out.display())] };
        e.speak("hello owner").await.unwrap();
        assert_eq!(std::fs::read_to_string(out).unwrap(), "hello owner");
        let bad = CommandEngine { argv: vec!["/bin/sh".into(), "-c".into(), "exit 3".into()] };
        assert!(bad.speak("x").await.is_err());
    }
}
