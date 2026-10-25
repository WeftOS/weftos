//! E-ink glance panel (ADR-119 S4): derive a five-field status line from the
//! `cos.v1` event stream and push it to a serial-attached e-ink board.
//!
//! `Glance::apply` is pure (events in, fields out) so the derivation is unit
//! tested from a recorded stream. `run` is the bridge: SSE in, one JSON line
//! out per change (debounced), heartbeat so the panel can tell "idle" from
//! "no link".

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::events::{AgentState, CardKind, Event};

/// Longest string the bridge sends for any free-text field; the firmware
/// truncates again to what fits.
const MAX_FIELD: usize = 120;
const DEBOUNCE: Duration = Duration::from_secs(2);
const HEARTBEAT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Glance {
    pub v: u8,
    pub state: String,
    pub task: String,
    pub next: String,
    pub alert: String,
    pub model: String,
    pub ts: String,
    #[serde(skip)]
    guide_active: bool,
    #[serde(skip)]
    blocked_reason: String,
    #[serde(skip)]
    tool_alert: String,
    #[serde(skip)]
    base_task: String,
    #[serde(skip)]
    base_next: String,
    #[serde(skip)]
    card: Option<LiveCard>,
}

/// A `glance.card` card: owns `task`/`next`/`alert` until it expires.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LiveCard {
    kind: CardKind,
    title: String,
    detail: String,
    alert: String,
    expires: DateTime<Utc>,
}

impl Default for Glance {
    fn default() -> Self {
        Self {
            v: 1,
            state: "idle".into(),
            task: String::new(),
            next: String::new(),
            alert: String::new(),
            model: String::new(),
            ts: String::new(),
            guide_active: false,
            blocked_reason: String::new(),
            tool_alert: String::new(),
            base_task: String::new(),
            base_next: String::new(),
            card: None,
        }
    }
}

impl Glance {
    /// Fold one event into the glance. Unknown shapes never reach here
    /// (the stream parser drops them), so every arm is a deliberate rule.
    pub fn apply(&mut self, ev: &Event) {
        self.apply_at(ev, Utc::now());
    }

    /// [`Glance::apply`] with an explicit clock, for tests and replays.
    pub fn apply_at(&mut self, ev: &Event, now: DateTime<Utc>) {
        match ev {
            Event::State { state, reason } => {
                self.state = match state {
                    AgentState::Idle => "idle",
                    AgentState::Listening => "listening",
                    AgentState::Thinking => "thinking",
                    AgentState::Speaking => "speaking",
                    AgentState::Blocked => "blocked",
                }
                .into();
                self.blocked_reason = match (state, reason) {
                    (AgentState::Blocked, Some(r)) => r.clone(),
                    (AgentState::Blocked, None) => "blocked".into(),
                    _ => String::new(),
                };
                if matches!(state, AgentState::Thinking | AgentState::Listening) {
                    // A new turn starts: last turn's tool alert is stale.
                    self.tool_alert.clear();
                }
            }
            Event::Status { task, model, .. } => {
                if !self.guide_active {
                    self.base_task = task.clone();
                }
                self.model = model.clone();
            }
            Event::Tool { name, summary, status } => match status.as_str() {
                "awaiting_confirmation" => self.tool_alert = format!("confirm: {summary}"),
                "error" | "refused" => self.tool_alert = format!("{name}: {status}"),
                _ => {}
            },
            Event::Steps { title, steps, current, .. } => {
                self.guide_active = true;
                self.base_task = title.clone();
                self.base_next = steps.get(*current).cloned().unwrap_or_else(|| "done".into());
            }
            Event::Checklist { title, items, .. } => {
                self.guide_active = true;
                self.base_task = title.clone();
                self.base_next = items
                    .iter()
                    .find(|i| !i.done)
                    .map(|i| i.text.clone())
                    .unwrap_or_else(|| "all done".into());
            }
            Event::Ask { question, .. } => {
                self.guide_active = true;
                self.base_next = question.clone();
            }
            Event::Message { text, is_final: true, .. } if !self.guide_active => {
                self.base_next = text.clone();
            }
            Event::Card { card: CardKind::Clear, .. } => self.card = None,
            Event::Card { card, title, detail, alert, ttl_minutes, .. } => {
                let ttl = if *ttl_minutes == 0 { card.default_ttl_minutes() } else { *ttl_minutes };
                self.card = Some(LiveCard {
                    kind: *card,
                    title: title.clone(),
                    detail: detail.clone(),
                    alert: alert.clone(),
                    expires: now + chrono::Duration::minutes(i64::from(ttl)),
                });
            }
            _ => {}
        }
        self.render(now);
    }

    /// Expire a card whose time is up. Returns true when it just went, so
    /// the bridge can redraw: expiry has no event of its own.
    pub fn tick(&mut self, now: DateTime<Utc>) -> bool {
        let expired = self.card.as_ref().is_some_and(|c| now >= c.expires);
        if expired {
            self.render(now);
        }
        expired
    }

    /// Recompute the displayed `task`/`next`/`alert` from the base fields,
    /// the live card (if any) and the alerts. Agent state is never touched.
    fn render(&mut self, now: DateTime<Utc>) {
        if self.card.as_ref().is_some_and(|c| now >= c.expires) {
            self.card = None;
        }
        let (task, next, card_alert) = match &self.card {
            Some(c) => (format!("{}: {}", c.kind.prefix(), c.title), c.detail.clone(), Some(c.alert.clone())),
            None => (self.base_task.clone(), self.base_next.clone(), None),
        };
        self.task = task;
        self.next = next;
        self.alert = if !self.blocked_reason.is_empty() {
            self.blocked_reason.clone()
        } else {
            card_alert.unwrap_or_else(|| self.tool_alert.clone())
        };
    }

    /// The line sent to the board: ASCII-only, bounded, newline-terminated.
    pub fn to_line(&self, ts: &str) -> String {
        let mut out = self.clone();
        for f in [&mut out.task, &mut out.next, &mut out.alert, &mut out.model] {
            *f = ascii_clip(f, MAX_FIELD);
        }
        out.ts = ts.to_string();
        let mut s = serde_json::to_string(&out).expect("glance serializes");
        s.push('\n');
        s
    }

    /// Everything but the clock: what decides whether the panel must redraw.
    fn content(&self) -> (&str, &str, &str, &str, &str) {
        (&self.state, &self.task, &self.next, &self.alert, &self.model)
    }
}

/// Map to printable ASCII (curly quotes and dashes folded, the rest `?`),
/// collapse whitespace, clip with an ellipsis of three dots.
pub fn ascii_clip(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut last_space = true;
    for c in s.chars() {
        let c = match c {
            '\u{2018}' | '\u{2019}' => '\'',
            '\u{201c}' | '\u{201d}' => '"',
            '\u{2013}' | '\u{2014}' => '-',
            '\u{2026}' => '.',
            c if c.is_whitespace() => ' ',
            c if c.is_ascii() && !c.is_ascii_control() => c,
            _ => '?',
        };
        if c == ' ' {
            if last_space {
                continue;
            }
            last_space = true;
        } else {
            last_space = false;
        }
        out.push(c);
    }
    let out = out.trim().to_string();
    if out.len() > max {
        format!("{}...", out[..max.saturating_sub(3)].trim_end())
    } else {
        out
    }
}

/// Incremental SSE parser: feed chunks, get the events of complete `data:`
/// lines. Keep-alives and unparseable or unknown-kind lines are dropped
/// (contract: a client ignores kinds it does not know).
#[derive(Default)]
pub struct SseParser {
    buf: String,
}

impl SseParser {
    pub fn feed(&mut self, chunk: &str) -> Vec<Event> {
        self.buf.push_str(chunk);
        let mut out = Vec::new();
        while let Some(i) = self.buf.find('\n') {
            let line: String = self.buf.drain(..=i).collect();
            if let Some(data) = line.trim_end().strip_prefix("data:")
                && let Ok(ev) = serde_json::from_str::<Event>(data.trim())
            {
                out.push(ev);
            }
        }
        out
    }
}

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
}

/// What `devices.status` learns about this process: best effort, never fatal.
fn write_status(path: &Path, port: &Path, port_open: bool, last_write: Option<&str>, last_error: Option<&str>) {
    let body = serde_json::json!({
        "pid": std::process::id(),
        "port": port.display().to_string(),
        "port_open": port_open,
        "updated_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "last_write_at": last_write,
        "last_error": last_error,
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

pub async fn run(args: GlanceSerialArgs) -> anyhow::Result<()> {
    use std::io::Write;
    let (tx, mut rx) = mpsc::channel(256);
    tokio::spawn(read_events(args.url.trim_end_matches('/').to_string(), tx));

    let mut g = Glance::default();
    let mut port: Option<std::fs::File> = None;
    let mut sent: Option<Glance> = None;
    let mut dirty = true;
    let mut deadline = Instant::now() + DEBOUNCE;
    let mut last_write = Instant::now() - HEARTBEAT;
    let status_path = args.status_file.clone().unwrap_or_else(|| crate::config::DevicesConfig::default().glance_status);
    let mut last_write_at: Option<String> = None;
    let mut last_error: Option<String> = None;
    loop {
        write_status(&status_path, &args.port, port.is_some(), last_write_at.as_deref(), last_error.as_deref());
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(Some(ev)) => { g.apply(&ev); dirty = true; deadline = Instant::now() + DEBOUNCE; }
                Some(None) => {
                    g.state = "blocked".into();
                    g.blocked_reason = "agent link down".into();
                    g.render(Utc::now());
                    dirty = true;
                    deadline = Instant::now() + DEBOUNCE;
                }
                None => return Ok(()),
            },
            _ = tokio::time::sleep_until(deadline.max(Instant::now())), if dirty => {}
            _ = tokio::time::sleep(Duration::from_secs(5)) => {}
        }
        if g.tick(Utc::now()) {
            dirty = true;
            deadline = Instant::now() + DEBOUNCE;
        }
        let now = Instant::now();
        let changed = sent.as_ref().map(|s| s.content() != g.content()).unwrap_or(true);
        let due = (dirty && changed && now >= deadline) || now.duration_since(last_write) >= HEARTBEAT;
        if dirty && !changed {
            dirty = false;
        }
        if !due {
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
        let line = g.to_line(&chrono::Local::now().format("%H:%M").to_string());
        let f = port.as_mut().expect("opened above");
        if let Err(e) = f.write_all(line.as_bytes()).and_then(|_| f.flush()) {
            tracing::warn!("serial write: {e}; reopening");
            last_error = Some(e.to_string());
            port = None;
            sent = None;
            deadline = now + Duration::from_secs(3);
            continue;
        }
        tracing::info!("glance: {}", line.trim_end());
        sent = Some(g.clone());
        dirty = false;
        last_write = now;
        last_write_at = Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
        last_error = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A recorded turn: thinking, status, a guide, a tool refusal, reply, idle.
    const STREAM: &str = r#":keep-alive
data: {"kind":"state","state":"thinking"}

data: {"kind":"status","task":"voice turn","model":"local/glm","lanes_running":0}

data: {"kind":"future-kind","x":1}

data: {"kind":"steps","id":"flash","title":"Flash the pad","steps":["Plug it in","Run the flasher"],"current":1}

data: {"kind":"tool","name":"fs.write","summary":"write file","status":"refused"}

data: {"kind":"state","state":"speaking"}

data: {"kind":"message","turn_id":"0b3f7c1e5a9d4e2f8c6a1d7b3e9f5a20","text":"Done","final":true}

data: {"kind":"state","state":"idle"}

"#;

    fn replay(chunks: &[&str]) -> Vec<Glance> {
        let mut p = SseParser::default();
        let mut g = Glance::default();
        let mut snaps = Vec::new();
        for c in chunks {
            for ev in p.feed(c) {
                g.apply(&ev);
                snaps.push(g.clone());
            }
        }
        snaps
    }

    #[test]
    fn derives_fields_from_recorded_stream() {
        let snaps = replay(&[STREAM]);
        // The unknown kind is dropped: 7 known events.
        assert_eq!(snaps.len(), 7);
        let thinking = &snaps[1];
        assert_eq!((thinking.state.as_str(), thinking.task.as_str(), thinking.model.as_str()), ("thinking", "voice turn", "local/glm"));
        let guide = &snaps[2];
        assert_eq!((guide.task.as_str(), guide.next.as_str()), ("Flash the pad", "Run the flasher"));
        assert_eq!(snaps[3].alert, "fs.write: refused");
        // The final message does not override a guide's next step.
        assert_eq!(snaps[5].next, "Run the flasher");
        assert_eq!(snaps[6].state, "idle");
        // Tool alert stays until the next turn begins.
        assert_eq!(snaps[6].alert, "fs.write: refused");
    }

    #[test]
    fn blocked_reason_is_the_alert_and_clears() {
        let mut g = Glance::default();
        g.apply(&Event::blocked("model: connection refused"));
        assert_eq!((g.state.as_str(), g.alert.as_str()), ("blocked", "model: connection refused"));
        g.apply(&Event::state(AgentState::Thinking));
        assert_eq!(g.alert, "");
    }

    #[test]
    fn checklist_next_is_first_undone() {
        let mut p = SseParser::default();
        let evs = p.feed("data: {\"kind\":\"checklist\",\"id\":\"d\",\"title\":\"Today\",\"items\":[{\"id\":\"a\",\"text\":\"one\",\"done\":true},{\"id\":\"b\",\"text\":\"two\",\"done\":false}]}\n");
        let mut g = Glance::default();
        g.apply(&evs[0]);
        assert_eq!((g.task.as_str(), g.next.as_str()), ("Today", "two"));
    }

    #[test]
    fn parser_handles_split_chunks_and_malformed_lines() {
        let snaps = replay(&["data: {\"kind\":\"state\",", "\"state\":\"speaking\"}\ndata: {not json}\n", "data: {\"kind\":\"state\",\"state\":\"idle\"}\n"]);
        assert_eq!(snaps.len(), 2);
        assert_eq!(snaps[0].state, "speaking");
    }

    #[test]
    fn line_is_ascii_bounded_and_one_line() {
        let g = Glance { task: format!("caf\u{e9} \u{201c}x\u{201d}\n{}", "y".repeat(300)), ..Glance::default() };
        let line = g.to_line("09:05");
        assert!(line.is_ascii() && line.ends_with('\n') && line.matches('\n').count() == 1);
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["ts"], "09:05");
        let task = v["task"].as_str().unwrap();
        assert!(task.starts_with("caf? \"x\" y") && task.len() <= MAX_FIELD && task.ends_with("..."));
    }
}
